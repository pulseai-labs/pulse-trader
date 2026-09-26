//! The one publish step the data-ops verbs share (issue #259): a rename that
//! makes a file visible is followed by an fsync of the **directory** it landed
//! in — and, when the run created that directory (or any level above it), of
//! every level it created up to and including the first ancestor that already
//! existed.
//!
//! A rename is atomic, but it is not durable by itself: the entry it creates
//! lives in the parent directory, and only an fsync of THAT directory carries
//! the entry to stable storage. The store's own publish already does this for
//! snapshots and `HEAD` pointers (`publish_atomically` / `write_head`); the
//! import/backup surface did not — the snapshot copies, the installed database,
//! the backup database and the backup's own `HEAD` manifest all renamed without
//! it. A power loss after `pulse backup` printed success could therefore keep
//! the database and its manifest while losing a snapshot they reference, which
//! leaves a backup the restore can only refuse; an import could keep the
//! target's name while losing the target.
//!
//! Creating `candles/<PAIR>/<TF>/` writes an entry into `candles/<PAIR>/`, into
//! `candles/` and adds the file to `<TF>/`, so every level a run created is
//! synced in ITS parent — the nested entries, not just the file, are what #259
//! is about. Below a relative path with no existing first component, the entry
//! lands in the current directory, which is what gets synced.

use std::fs;
use std::path::{Path, PathBuf};

/// The directory a published path landed in. A path with no parent at all (a
/// bare file name) publishes into the current directory: `create_dir_all("")`
/// and `File::open("")` both fail, so the empty parent maps to `.`.
fn parent_dir(published: &Path) -> PathBuf {
    match published.parent() {
        Some(dir) if !dir.as_os_str().is_empty() => dir.to_path_buf(),
        _ => PathBuf::from("."),
    }
}

/// The directory that lexically holds `dir`: its parent, with a RELATIVE path's
/// empty leading component resolved to the current directory (`.`). A relative
/// path's first component lives in the CWD, and that is the entry a copy of it
/// creates — so the walk below never loses its last level. `None` only at the
/// filesystem root.
fn holding_dir(dir: &Path) -> Option<PathBuf> {
    match dir.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => Some(parent.to_path_buf()),
        Some(_) => Some(PathBuf::from(".")),
        None => None,
    }
}

/// The deepest ancestor of `path` (inclusive) that already exists — everything
/// strictly below it is what `create_dir_all(path)` is about to create.
///
/// A relative path whose first component is absent resolves to `.`: the entry
/// that component creates lands in the current directory, so the walk must end
/// there rather than at nothing.
pub(crate) fn existing_ancestor(path: &Path) -> Option<PathBuf> {
    let mut candidate = Some(path.to_path_buf());
    while let Some(dir) = candidate {
        if dir.is_dir() {
            return Some(dir);
        }
        // `holding_dir` maps a relative path's empty parent to `.`, so this walk
        // ends at the current directory; the `next != dir` guard keeps it total
        // (`.` holds itself).
        candidate = match holding_dir(&dir) {
            Some(next) if next != dir => Some(next),
            _ => None,
        };
    }
    None
}

/// The directories a publish must sync, deepest first: the published path's
/// parent, then every level above it up to and INCLUDING `created` — the first
/// ancestor that existed before the run made any directory.
///
/// The pre-existing ancestor is included because the first level the run created
/// is an entry in it; nothing above it was touched, so the walk stops there.
pub(crate) fn levels_to_sync(published: &Path, created: Option<&Path>) -> Vec<PathBuf> {
    let parent = parent_dir(published);
    let mut levels = vec![parent.clone()];
    if let Some(root) = created
        && parent != root
    {
        let mut current = holding_dir(&parent);
        while let Some(dir) = current {
            levels.push(dir.clone());
            if dir == root || dir == Path::new(".") || dir.parent().is_none() {
                break;
            }
            current = holding_dir(&dir);
        }
    }
    levels
}

/// fsync one directory, so the entries it holds reach stable storage.
///
/// # Errors
///
/// Returns an [`anyhow::Error`] naming the directory when it cannot be opened
/// or synced. A publish that could not be made durable is reported, never
/// swallowed: `pulse backup` would otherwise print success over it (issue
/// #259).
pub(crate) fn sync_dir(dir: &Path) -> anyhow::Result<()> {
    #[cfg(test)]
    if probe::take_injected_failure(dir) {
        return Err(anyhow::anyhow!(
            "fsync directory {}: injected failure (cfg(test) seam)",
            dir.display()
        ));
    }
    fs::File::open(dir)
        .and_then(|handle| handle.sync_all())
        .map_err(|error| anyhow::anyhow!("fsync directory {}: {error}", dir.display()))
}

/// fsync a just-written FILE — its bytes — before the rename that gives it its
/// final name.
///
/// `VACUUM INTO` does not guarantee its output is on disk, and every copy this
/// crate publishes goes through a rename that promises the bytes by name: the
/// name must not be durable before the bytes are (fix round 1, F5).
/// `destination` is the published path the copy is about to take, which the test
/// seam records alongside the sync (it must NOT exist yet).
///
/// # Errors
///
/// Returns an [`anyhow::Error`] naming the file when it cannot be opened or
/// synced.
pub(crate) fn sync_file(file: &Path, destination: &Path) -> anyhow::Result<()> {
    fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(file)
        .and_then(|handle| handle.sync_all())
        .map_err(|error| anyhow::anyhow!("fsync file {}: {error}", file.display()))?;
    #[cfg(test)]
    probe::record(probe::SyncKind::File, file, destination);
    #[cfg(not(test))]
    let _ = destination;
    Ok(())
}

/// fsync the directories a just-renamed `published` path needs (issue #259):
/// see [`levels_to_sync`] for which they are and why.
///
/// # Errors
///
/// Returns an [`anyhow::Error`] when any level cannot be opened or synced.
pub(crate) fn sync_published(published: &Path, created: Option<&Path>) -> anyhow::Result<()> {
    for dir in levels_to_sync(published, created) {
        sync_dir(&dir)?;
        #[cfg(test)]
        probe::record(probe::SyncKind::Dir, &dir, published);
    }
    Ok(())
}

/// The recording seam the durability tests read.
///
/// `cfg(test)`-only. Every publish records one event per sync — the file's own
/// bytes before its rename, each directory after it — on the calling thread, so
/// a test can assert WHICH paths were synced and WHEN: `destination_present` is
/// the state of the published path at the moment of the sync, so `false` on a
/// file sync proves it ran before the rename and `true` on a directory sync
/// proves it ran after.
///
/// [`probe::fail_next_publish_of`] is the other half: it makes one publish's
/// first directory sync fail, which is how the rollback tests reach the
/// post-rename failure path (fix round 1, F1/F2/F8) without weakening anything
/// in production.
#[cfg(test)]
pub(crate) mod probe {
    use std::cell::RefCell;
    use std::path::{Path, PathBuf};

    /// Which kind of sync an event records.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub(crate) enum SyncKind {
        /// The published file's own bytes, before its rename.
        File,
        /// A directory the publish landed in (or created), after the rename.
        Dir,
    }

    /// One recorded sync.
    #[derive(Debug, Clone, PartialEq, Eq)]
    pub(crate) struct SyncEvent {
        /// File sync or directory sync.
        pub(crate) kind: SyncKind,
        /// The file or directory the fsync ran on.
        pub(crate) path: PathBuf,
        /// The published destination this sync belongs to.
        pub(crate) destination: PathBuf,
        /// `destination.exists()` at the moment of the sync.
        pub(crate) destination_present: bool,
    }

    thread_local! {
        /// Events recorded on THIS thread. The publish sites are synchronous
        /// functions and the tests that read this run on a current-thread
        /// runtime, so a test sees exactly its own calls.
        static SYNCS: RefCell<Vec<SyncEvent>> = const { RefCell::new(Vec::new()) };

        /// The directory whose next sync must fail, one-shot.
        static FAIL_NEXT_SYNC_DIR: RefCell<Option<PathBuf>> = const { RefCell::new(None) };
    }

    /// Record one sync, with the published path's state at that moment.
    pub(crate) fn record(kind: SyncKind, path: &Path, destination: &Path) {
        SYNCS.with(|syncs| {
            syncs.borrow_mut().push(SyncEvent {
                kind,
                path: path.to_path_buf(),
                destination: destination.to_path_buf(),
                destination_present: destination.exists(),
            });
        });
    }

    /// Take every event recorded on this thread so far (leaving it empty).
    pub(crate) fn take() -> Vec<SyncEvent> {
        SYNCS.with(|syncs| std::mem::take(&mut *syncs.borrow_mut()))
    }

    thread_local! {
        /// The sidecars the install MOVED ASIDE beside a target, in order.
        static QUARANTINED: RefCell<Vec<PathBuf>> = const { RefCell::new(Vec::new()) };
    }

    /// Record the sidecars an install quarantined (fix round 1, F3).
    ///
    /// A successful install ends with those files gone whether or not they were
    /// moved aside — the WAL switch that follows reopens the database and cleans
    /// a stale sidecar up on its own — so the observable end state cannot tell
    /// the two apart. What proves the mechanism is that they were taken out of
    /// the way BEFORE the new file was renamed in, which is what this records.
    pub(crate) fn record_quarantine(moved: &[PathBuf]) {
        QUARANTINED.with(|quarantined| quarantined.borrow_mut().extend_from_slice(moved));
    }

    /// Take every quarantined path recorded on this thread so far.
    pub(crate) fn take_quarantine() -> Vec<PathBuf> {
        QUARANTINED.with(|quarantined| std::mem::take(&mut *quarantined.borrow_mut()))
    }

    /// Make the NEXT sync of `dir` fail, one-shot.
    ///
    /// This is the post-rename failure the rollback tests need: a publish's
    /// directory sync runs after its rename, so an install or a snapshot copy
    /// can be driven into the "the file landed but the publish is unconfirmed"
    /// state deterministically (fix round 1, F1/F2/F8).
    pub(crate) fn fail_next_sync_of(dir: &Path) {
        FAIL_NEXT_SYNC_DIR.with(|pending| *pending.borrow_mut() = Some(dir.to_path_buf()));
    }

    /// Consume the pending injected failure when it names `dir`.
    pub(crate) fn take_injected_failure(dir: &Path) -> bool {
        FAIL_NEXT_SYNC_DIR.with(|pending| {
            let mut pending = pending.borrow_mut();
            if pending.as_deref() == Some(dir) {
                *pending = None;
                return true;
            }
            false
        })
    }
}
