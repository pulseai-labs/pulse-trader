//! `engine_fingerprint_source` — r2.s3.w1 (#155), spec AC-1.
//!
//! `build.rs` folds a sha2-256 of the engine source set — input (d) — into
//! `PULSE_ENGINE_FINGERPRINT`. This suite exercises the exact function and root
//! list `build.rs` runs: both sides `include!` the same `build_support/` files,
//! so the test can never drift from what the build actually hashes.
//!
//! Properties covered (spec AC-1, over a temp tree this suite writes itself):
//!   i.   the same tree hashes identically on repeated calls;
//!   ii.  a one-byte change in an in-set `.rs` file changes the hex;
//!   iii. an added in-set `.rs` file changes the hex;
//!   iv.  an out-of-set change, and a non-`.rs` file under an in-set root, do not;
//!   v.   the hex is independent of the order the walker visits entries;
//!   vi.  `ENGINE_SOURCE_ROOTS` is the literal eight-root list the spec locks
//!        (L2) — asserted here so an edit cannot silently drift from the build;
//!   viii. the relative path bytes are in the frame — swapping two in-set
//!        files' contents, or moving identical bytes to a different in-set
//!        path, changes the hex;
//!   ix.  the `<len as u64 LE>` field is in the frame — a byte shifted across a
//!        file boundary changes the hex, and a fixed tree's digest matches a
//!        golden value.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::Path;

// The same files `build.rs` includes — the build and this suite share the root
// list and the hashing function byte-for-byte (property vi).
include!("../build_support/engine_source_set.rs");
include!("../build_support/source_tree_hash.rs");

/// The spec's locked root list (SPINE.md L2): four directories plus four single
/// files. Written out literally so `roots_match_the_locked_list_and_exist`
/// fails on any drift in `build_support/engine_source_set.rs`.
const LOCKED_ROOTS: &[&str] = &[
    "src/domain/backtest",
    "src/adapters/backtest",
    "src/adapters/indicators",
    "src/domain/dsl",
    "src/domain/indicator.rs",
    "src/domain/series.rs",
    "src/domain/candle.rs",
    "src/domain/sizing.rs",
];

/// The mini-tree every property test starts from: one `.rs` file per directory
/// root, the four file roots, an out-of-set `.rs` control, and a non-`.rs`
/// control under an in-set root.
const TREE_FILES: &[(&str, &str)] = &[
    ("src/domain/backtest/engine.rs", "pub fn a() {}\n"),
    ("src/domain/backtest/stats.rs", "pub fn b() {}\n"),
    ("src/adapters/backtest/engine.rs", "pub fn c() {}\n"),
    ("src/adapters/indicators/ema.rs", "pub fn d() {}\n"),
    ("src/domain/dsl/strategy.rs", "pub fn e() {}\n"),
    ("src/domain/indicator.rs", "pub fn f() {}\n"),
    ("src/domain/series.rs", "pub fn g() {}\n"),
    ("src/domain/candle.rs", "pub fn h() {}\n"),
    ("src/domain/sizing.rs", "pub fn i() {}\n"),
    ("src/application/backtest.rs", "pub fn outside() {}\n"),
    ("src/domain/backtest/NOTES.md", "not rust\n"),
];

/// The nine in-set `.rs` files of `TREE_FILES`, as relative paths — the exact
/// set `source_tree_files` must enumerate on the mini-tree.
const IN_SET_FILES: &[&str] = &[
    "src/adapters/backtest/engine.rs",
    "src/adapters/indicators/ema.rs",
    "src/domain/backtest/engine.rs",
    "src/domain/backtest/stats.rs",
    "src/domain/candle.rs",
    "src/domain/dsl/strategy.rs",
    "src/domain/indicator.rs",
    "src/domain/series.rs",
    "src/domain/sizing.rs",
];

fn write_file(base: &Path, rel: &str, contents: &str) {
    let path = base.join(rel);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, contents).unwrap();
}

fn write_tree(base: &Path, files: &[(&str, &str)]) {
    for (rel, contents) in files {
        write_file(base, rel, contents);
    }
}

/// Creates all eight locked roots under `base` — the four directories empty,
/// the four file roots as files holding `body` — so `hash` can run on a
/// minimal probe tree without tripping the missing-root build error.
fn scaffold_roots(base: &Path, body: &str) {
    for root in ENGINE_SOURCE_ROOTS {
        if std::path::Path::new(root)
            .extension()
            .is_some_and(|ext| ext.eq_ignore_ascii_case("rs"))
        {
            write_file(base, root, body);
        } else {
            std::fs::create_dir_all(base.join(root)).unwrap();
        }
    }
}

fn hash(dir: &tempfile::TempDir) -> String {
    source_tree_hash(dir.path(), ENGINE_SOURCE_ROOTS)
}

/// (i) The same tree hashes identically on repeated calls, and the digest is
/// sha2-256 lowercase hex.
#[test]
fn same_tree_same_hash() {
    let tmp = tempfile::tempdir().unwrap();
    write_tree(tmp.path(), TREE_FILES);
    let first = hash(&tmp);
    let second = hash(&tmp);
    assert_eq!(first, second, "same tree must hash identically");
    assert_eq!(first.len(), 64, "sha2-256 hex is 64 chars, got {first:?}");
    assert!(
        first
            .chars()
            .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()),
        "digest must be lowercase hex, got {first:?}"
    );
}

/// (ii) A one-byte change in an in-set file changes the digest.
#[test]
fn byte_change_in_set_changes_hash() {
    let tmp = tempfile::tempdir().unwrap();
    write_tree(tmp.path(), TREE_FILES);
    let before = hash(&tmp);
    write_file(
        tmp.path(),
        "src/domain/backtest/stats.rs",
        "pub fn c() {}\n",
    );
    assert_ne!(
        before,
        hash(&tmp),
        "a one-byte change in an in-set file must change the fingerprint"
    );
}

/// (iii) An added `.rs` file under an in-set root changes the digest.
#[test]
fn added_in_set_file_changes_hash() {
    let tmp = tempfile::tempdir().unwrap();
    write_tree(tmp.path(), TREE_FILES);
    let before = hash(&tmp);
    write_file(
        tmp.path(),
        "src/adapters/indicators/rsi.rs",
        "pub fn r() {}\n",
    );
    assert_ne!(
        before,
        hash(&tmp),
        "an added in-set .rs file must change the fingerprint"
    );
}

/// (iv) Changes outside the set — and non-`.rs` files under an in-set root —
/// leave the digest untouched.
#[test]
fn out_of_set_and_non_rs_changes_do_not_move_hash() {
    let tmp = tempfile::tempdir().unwrap();
    write_tree(tmp.path(), TREE_FILES);
    let before = hash(&tmp);
    write_file(
        tmp.path(),
        "src/application/backtest.rs",
        "pub fn changed() {}\n",
    );
    write_file(
        tmp.path(),
        "src/tauri/commands.rs",
        "pub fn also_outside() {}\n",
    );
    write_file(tmp.path(), "src/domain/backtest/NOTES.md", "changed\n");
    write_file(tmp.path(), "src/domain/dsl/readme.txt", "not rust\n");
    assert_eq!(
        before,
        hash(&tmp),
        "out-of-set and non-.rs changes must not move the fingerprint"
    );
}

/// (v) The digest depends on content, not on the order the filesystem hands
/// entries to the walker: the same tree written in reverse creation order
/// hashes identically.
#[test]
fn walk_order_does_not_change_hash() {
    let forward = tempfile::tempdir().unwrap();
    write_tree(forward.path(), TREE_FILES);
    let reversed: Vec<(&str, &str)> = TREE_FILES.iter().rev().copied().collect();
    let backward = tempfile::tempdir().unwrap();
    write_tree(backward.path(), &reversed);
    assert_eq!(
        hash(&forward),
        hash(&backward),
        "walk order must not affect the fingerprint"
    );
}

/// (vi) The root list the test hashes is the spec's locked list — the same
/// const `build.rs` folds — and every locked root exists in this checkout.
#[test]
fn roots_match_the_locked_list_and_exist() {
    assert_eq!(
        ENGINE_SOURCE_ROOTS, LOCKED_ROOTS,
        "ENGINE_SOURCE_ROOTS drifted from the locked L2 list"
    );
    let base = Path::new(env!("CARGO_MANIFEST_DIR"));
    for root in ENGINE_SOURCE_ROOTS {
        assert!(
            base.join(root).exists(),
            "locked engine source-set root missing from this checkout: {root}"
        );
    }
}

/// The enumeration feeding both the hasher and `cargo:rerun-if-changed` is
/// sorted by relative path bytes and covers exactly the in-set `.rs` files.
#[test]
fn enumeration_is_sorted_and_covers_the_set() {
    let tmp = tempfile::tempdir().unwrap();
    write_tree(tmp.path(), TREE_FILES);
    let files = source_tree_files(tmp.path(), ENGINE_SOURCE_ROOTS);
    let rels: Vec<&[u8]> = files.iter().map(|(rel, _)| rel.as_slice()).collect();
    let mut sorted = rels.clone();
    sorted.sort();
    assert_eq!(
        rels, sorted,
        "source_tree_files must return path-sorted rows"
    );
    let rels: Vec<String> = files
        .iter()
        .map(|(rel, _)| String::from_utf8(rel.clone()).unwrap())
        .collect();
    assert_eq!(
        rels, IN_SET_FILES,
        "the enumeration must cover exactly the in-set .rs files"
    );
}

/// (viii) The relative path bytes are in the frame: swapping the contents of
/// two in-set files changes the digest, and moving identical bytes to a
/// different in-set path changes it too. The moved-identical-bytes case is the
/// strict pin for the path feed — a mutant that stops feeding
/// `<relative path bytes>` sees an unchanged `(len, contents)` stream there
/// and produces an unchanged hash, so this test fails against it.
#[test]
fn path_bytes_are_in_the_frame() {
    let original = tempfile::tempdir().unwrap();
    scaffold_roots(original.path(), "");
    write_file(
        original.path(),
        "src/domain/backtest/engine.rs",
        "pub fn a() {}\n",
    );
    write_file(
        original.path(),
        "src/domain/backtest/stats.rs",
        "pub fn b() {}\n",
    );
    let swapped = tempfile::tempdir().unwrap();
    scaffold_roots(swapped.path(), "");
    write_file(
        swapped.path(),
        "src/domain/backtest/engine.rs",
        "pub fn b() {}\n",
    );
    write_file(
        swapped.path(),
        "src/domain/backtest/stats.rs",
        "pub fn a() {}\n",
    );
    assert_ne!(
        hash(&original),
        hash(&swapped),
        "swapping two in-set files' contents must change the fingerprint"
    );

    // Every hashed file carries identical bytes, so both trees feed the same
    // `(len, contents)` stream in path order — only the path bytes in the
    // frame distinguish them.
    let moved_a = tempfile::tempdir().unwrap();
    scaffold_roots(moved_a.path(), "same bytes\n");
    write_file(
        moved_a.path(),
        "src/domain/backtest/engine.rs",
        "same bytes\n",
    );
    write_file(
        moved_a.path(),
        "src/domain/backtest/stats.rs",
        "same bytes\n",
    );
    let moved_b = tempfile::tempdir().unwrap();
    scaffold_roots(moved_b.path(), "same bytes\n");
    write_file(
        moved_b.path(),
        "src/domain/backtest/engine.rs",
        "same bytes\n",
    );
    write_file(moved_b.path(), "src/domain/dsl/strategy.rs", "same bytes\n");
    assert_ne!(
        hash(&moved_a),
        hash(&moved_b),
        "the same bytes at a different in-set path must change the fingerprint"
    );
}

/// (ix) The u64-LE length prefix is in the frame: shifting one byte across a
/// file boundary changes the digest, and a fixed tree's digest matches a
/// golden value. The golden assert is the pin — with `0x00` still delimiting
/// fields, a mutant that drops `<len as u64 LE>` still moves its hash on the
/// boundary shift, but no longer produces this digest.
#[test]
fn length_prefix_is_in_the_frame() {
    let roots: &[&str] = &["pkg"];
    let packed = tempfile::tempdir().unwrap();
    write_file(packed.path(), "pkg/a.rs", "ab");
    write_file(packed.path(), "pkg/b.rs", "c");
    let shifted = tempfile::tempdir().unwrap();
    write_file(shifted.path(), "pkg/a.rs", "a");
    write_file(shifted.path(), "pkg/b.rs", "bc");
    let digest = source_tree_hash(packed.path(), roots);
    assert_ne!(
        digest,
        source_tree_hash(shifted.path(), roots),
        "a byte shifted across a file boundary must change the fingerprint"
    );
    assert_eq!(
        digest, "ba64c17c808c089339897e39a5e6cab51395193adc8364eb136506ba317b9391",
        "the frame digest must match the golden value — a dropped length field changes it"
    );
}

/// A missing root is a build error, not an empty contribution.
#[test]
#[should_panic(expected = "engine source-set root does not exist")]
fn missing_root_is_a_build_error() {
    let tmp = tempfile::tempdir().unwrap();
    let _ = source_tree_hash(tmp.path(), &["src/domain/definitely_missing"]);
}

/// An empty root directory is legal and hashes nothing — the digest is the
/// sha2-256 of empty input.
#[test]
fn empty_root_is_legal() {
    let tmp = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(tmp.path().join("src/domain/backtest")).unwrap();
    assert_eq!(
        source_tree_hash(tmp.path(), &["src/domain/backtest"]),
        "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855",
        "an empty root must hash to the sha2-256 of empty input"
    );
}

/// Sanity on the real checkout: the locked roots hash to a stable sha2-256 hex.
#[test]
fn real_tree_hashes_stably() {
    let base = Path::new(env!("CARGO_MANIFEST_DIR"));
    let first = source_tree_hash(base, ENGINE_SOURCE_ROOTS);
    let second = source_tree_hash(base, ENGINE_SOURCE_ROOTS);
    assert_eq!(first, second, "the real tree must hash stably");
    assert_eq!(first.len(), 64, "sha2-256 hex is 64 chars, got {first:?}");
}
