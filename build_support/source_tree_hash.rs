// `source_tree_hash` — the hasher behind `engine_fingerprint` input (d)
// (r2.s3.w1, #155).
//
// `include!`'d by BOTH `build.rs` (which folds the result after input (c),
// behind the `b"engine-source-v1\0"` domain prefix) and
// `tests/engine_fingerprint_source.rs` (which exercises this exact function),
// so the build and the test can never drift on the hashing.
//
// NOTE: `include!`'d as a raw token stream — no `//!` inner docs, no `use`, no
// inner attributes. Every path below is fully qualified so the file cannot
// collide with an includer's own imports (`build.rs` already has
// `use sha2::{Digest, Sha256}`); trait calls go through UFCS for the same
// reason. `Path` in the signatures resolves at the include site — both
// includers import it.

/// SHA-256 over the sorted (path-bytes ascending) `.rs` files under `roots`,
/// each fed as `<relative path bytes> 0x00 <len as u64 LE> <contents>`; returns
/// lowercase hex.
///
/// Relative paths use `/` separators regardless of host; sort order is raw
/// bytes, so walk order does not matter. A root may name a single `.rs` file or
/// a directory walked recursively; an empty root is legal (hashes nothing); a
/// missing root is a build error (panic).
fn source_tree_hash(base: &Path, roots: &[&str]) -> String {
    let mut hasher = <sha2::Sha256 as sha2::Digest>::new();
    for (path_bytes, rel_path) in source_tree_files(base, roots) {
        let contents = std::fs::read(base.join(&rel_path)).unwrap_or_else(|e| {
            panic!("failed to read engine source file {}: {e}", rel_path.display())
        });
        sha2::Digest::update(&mut hasher, &path_bytes);
        sha2::Digest::update(&mut hasher, [0u8]);
        sha2::Digest::update(&mut hasher, (contents.len() as u64).to_le_bytes());
        sha2::Digest::update(&mut hasher, &contents);
    }
    hex::encode(<sha2::Sha256 as sha2::Digest>::finalize(hasher))
}

/// Enumerate the in-set `.rs` files under `roots` as `(relative path bytes,
/// relative path)` pairs, sorted by raw path bytes ascending.
///
/// Sibling of [`source_tree_hash`]: `build.rs` reuses it for the
/// `cargo:rerun-if-changed` lines so the watched set cannot drift from the
/// hashed set. Missing roots panic here too — the enumeration is where the
/// error surfaces.
fn source_tree_files(base: &Path, roots: &[&str]) -> Vec<(Vec<u8>, std::path::PathBuf)> {
    let mut out = Vec::new();
    let mut dirs = Vec::new();
    for root in roots {
        let root_path = base.join(root);
        if root_path.is_dir() {
            collect_tree(std::path::Path::new(root), &root_path, &mut out, &mut dirs);
        } else if root_path.is_file() {
            let rel = std::path::PathBuf::from(root);
            out.push((rel_bytes(&rel), rel));
        } else {
            panic!(
                "engine source-set root does not exist: {root} (resolved under {})",
                base.display()
            );
        }
    }
    out.sort();
    out.dedup();
    out
}

/// Every DIRECTORY the enumeration walks: each directory root plus every
/// subdirectory beneath it, sorted, deduped. Empty for file roots.
///
/// `build.rs` watches these alongside the files, and it has to: a Cargo
/// `rerun-if-changed` on a directory is NOT recursive, so watching only the
/// roots misses a `.rs` file added or removed inside a NESTED subdirectory —
/// that changes the subdirectory's mtime, not the root's, and the new file is
/// not on the previous build's file list either. Watching every walked directory
/// covers adds and removals at any depth.
///
/// Missing roots panic, exactly as in [`source_tree_files`].
fn source_tree_dirs(base: &Path, roots: &[&str]) -> Vec<std::path::PathBuf> {
    let mut files = Vec::new();
    let mut dirs = Vec::new();
    for root in roots {
        let root_path = base.join(root);
        if root_path.is_dir() {
            dirs.push(std::path::PathBuf::from(root));
            collect_tree(std::path::Path::new(root), &root_path, &mut files, &mut dirs);
        } else if !root_path.is_file() {
            panic!(
                "engine source-set root does not exist: {root} (resolved under {})",
                base.display()
            );
        }
    }
    dirs.sort();
    dirs.dedup();
    dirs
}

/// The recursive walk both enumerations share: push every `.rs` file under
/// `abs_dir` into `files`, and every subdirectory into `dirs`, tracking each
/// entry's path relative to the tree base so the sort key never depends on where
/// the checkout lives.
fn collect_tree(
    rel_dir: &Path,
    abs_dir: &Path,
    files: &mut Vec<(Vec<u8>, std::path::PathBuf)>,
    dirs: &mut Vec<std::path::PathBuf>,
) {
    let entries = std::fs::read_dir(abs_dir).unwrap_or_else(|e| {
        panic!("failed to read engine source-set dir {}: {e}", abs_dir.display())
    });
    for entry in entries {
        let entry = entry.unwrap_or_else(|e| {
            panic!(
                "failed to read an entry under {}: {e}",
                abs_dir.display()
            )
        });
        let rel = rel_dir.join(entry.file_name());
        let abs = abs_dir.join(entry.file_name());
        if abs.is_dir() {
            dirs.push(rel.clone());
            collect_tree(&rel, &abs, files, dirs);
        } else if abs.is_file() && abs.extension().is_some_and(|ext| ext == "rs") {
            files.push((rel_bytes(&rel), rel));
        }
    }
}

/// A relative path as `/`-separated raw bytes — the hash input's path form on
/// every host.
fn rel_bytes(rel: &Path) -> Vec<u8> {
    let mut out = Vec::new();
    for (i, component) in rel.components().enumerate() {
        if i > 0 {
            out.push(b'/');
        }
        out.extend_from_slice(component.as_os_str().as_encoded_bytes());
    }
    out
}
