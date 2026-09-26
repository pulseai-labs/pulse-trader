//! The one publish step the data-ops verbs share (issue #259): a rename that
//! makes a file visible is followed by an fsync of the **directory** it landed
//! in.
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
//! The nested snapshot layout gets the same treatment: `create_dir_all`
//! creates `candles/<PAIR>/<TF>/`, and each of those levels is an entry in ITS
//! parent, so every level the copy created is fsynced too — the nested entries,
//! not just the file, are what #259 is about.

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

/// The deepest ancestor of `path` (inclusive) that already exists — everything
/// strictly below it is what `create_dir_all(path)` is about to create.
pub(crate) fn existing_ancestor(path: &Path) -> Option<PathBuf> {
    let mut candidate = Some(path.to_path_buf());
    while let Some(dir) = candidate {
        if dir.is_dir() {
            return Some(dir);
        }
        candidate = dir.parent().map(Path::to_path_buf);
    }
    None
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
    fs::File::open(dir)
        .and_then(|handle| handle.sync_all())
        .map_err(|error| anyhow::anyhow!("fsync directory {}: {error}", dir.display()))
}

/// fsync the directory a just-renamed `published` path landed in (issue #259).
///
/// `created` is the deepest ancestor that existed BEFORE the caller's
/// `create_dir_all` ran (`None` when the caller created nothing). Every level
/// between it and the published path's parent is fsynced as well — INCLUDING
/// `created` itself, because the first level the copy created is an entry
/// inside it. Creating `candles/<PAIR>/<TF>/` writes an entry into
/// `candles/<PAIR>/`, into `candles/` and adds the file to `<TF>/`: syncing only
/// the leaf leaves the nested levels' own entries at the mercy of a power loss,
/// which is what #259 is about.
///
/// # Errors
///
/// Returns an [`anyhow::Error`] when any level cannot be opened or synced.
pub(crate) fn sync_published(published: &Path, created: Option<&Path>) -> anyhow::Result<()> {
    let parent = parent_dir(published);
    let mut levels = vec![parent.clone()];
    if let Some(root) = created
        && parent != root
    {
        let mut current = parent.parent().map(Path::to_path_buf);
        while let Some(dir) = current {
            levels.push(dir.clone());
            // Stop AT the pre-existing ancestor (its first new entry needs the
            // sync too) and never walk past the filesystem root.
            if dir == root || dir.parent().is_none() {
                break;
            }
            current = dir.parent().map(Path::to_path_buf);
        }
    }
    for dir in &levels {
        sync_dir(dir)?;
        #[cfg(test)]
        probe::record(dir, published);
    }
    Ok(())
}

/// The recording seam the durability tests read.
///
/// `cfg(test)`-only. Every publish records one event per directory it fsynced,
/// on the calling thread, so a test can assert WHICH directories were synced and
/// that each sync happened AFTER the rename: `published_present` is the state of
/// the published path at the moment of the sync — `true` proves the rename came
/// first, and a publish that synced before its rename (or forgot the sync
/// entirely) records no such event. Remove a `sync_published` call from a
/// publish site and its test has nothing to assert against, which is what makes
/// those tests fail with the fix reverted.
#[cfg(test)]
pub(crate) mod probe {
    use std::cell::RefCell;
    use std::path::{Path, PathBuf};

    /// One recorded publish: the directory that was synced, and whether the
    /// published path existed at that moment.
    #[derive(Debug, Clone, PartialEq, Eq)]
    pub(crate) struct Sync {
        /// The directory the fsync ran on.
        pub(crate) dir: PathBuf,
        /// `published.exists()` when the sync ran — `true` proves the rename
        /// came first.
        pub(crate) published_present: bool,
    }

    thread_local! {
        /// Events recorded on THIS thread. Every publish site is a synchronous
        /// function, so a `#[test]` sees exactly its own calls and no other
        /// test's (the harness gives each test its own thread).
        static SYNCS: RefCell<Vec<Sync>> = const { RefCell::new(Vec::new()) };
    }

    /// Record one sync, with the published path's state at that moment.
    pub(crate) fn record(dir: &Path, published: &Path) {
        SYNCS.with(|syncs| {
            syncs.borrow_mut().push(Sync {
                dir: dir.to_path_buf(),
                published_present: published.exists(),
            });
        });
    }

    /// Take every event recorded on this thread so far (leaving it empty).
    pub(crate) fn take() -> Vec<Sync> {
        SYNCS.with(|syncs| std::mem::take(&mut *syncs.borrow_mut()))
    }
}
