//! Ports of the Go ingest tests (`ingest_test.go`, `driver_test.go`,
//! `scan_test.go`, `meta_test.go`, `amberignore_test.go`, `decode_test.go`).
//! Everything runs unprivileged; xattr cases skip (with a note) where the
//! filesystem refuses them, exactly as the Go tests do.

use std::collections::HashMap;
use std::ffi::CString;
use std::fs;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{Arc, mpsc};
use std::thread;

use tempfile::TempDir;

use crate::amberignore::Matcher;
use crate::chunkers::{ByteOpts, ItemChunker};
use crate::fstree::{Entry, decode_dir_leaf, decode_dir_node, decode_file_node};
use crate::key::{Key, Type};
use crate::packstore;

use super::driver::{ChanSink, Driver, MapSink};
use super::parallel::PBuilder;
use super::scan::scan_tree;
use super::{
    DEFAULT_ITEM_BITS, DEFAULT_XATTR_INLINE_MAX, Exclude, Opts, Progress, dir, meta, objects, scan,
    scan_with,
};

// ---------------------------------------------------------------------------
// Shared helpers (Go: ingest_test.go).
// ---------------------------------------------------------------------------

fn driver(ic: ItemChunker, byte_opts: Option<ByteOpts>, xattr_inline_max: usize) -> Driver {
    Driver {
        ic,
        byte_opts,
        xattr_inline_max,
        progress: None,
    }
}

/// Builds the tree at `dir` with the sequential driver and returns the root
/// plus a map of every emitted object's key to its bytes. It is the
/// reference oracle the parallel build is checked against (Go:
/// `collectSequential`).
fn collect_sequential(
    dir: &Path,
    ign: Option<&Matcher>,
    ic: ItemChunker,
    byte_opts: Option<ByteOpts>,
    xattr_inline_max: usize,
) -> (Key, HashMap<Key, Vec<u8>>) {
    let sink = MapSink::default();
    let d = driver(ic, byte_opts, xattr_inline_max);
    let root = d.build_dir(dir, ign, &sink).expect("sequential build");
    (root, sink.0.into_inner().unwrap())
}

/// Drains a parallel build (the production wiring used by `objects`) into a
/// key → bytes map and returns the root (Go: `collectParallel` +
/// `dirBuildRoot`).
fn collect_parallel(
    dir: &Path,
    ign: Option<&Matcher>,
    ic: ItemChunker,
    byte_opts: Option<ByteOpts>,
    xattr_inline_max: usize,
    jobs: usize,
) -> (Key, HashMap<Key, Vec<u8>>) {
    let jobs = jobs.max(1);
    let d = driver(ic, byte_opts, xattr_inline_max);
    let (tx, rx) = mpsc::sync_channel(jobs * 2);
    let mut objs = HashMap::new();
    let mut root: Option<Key> = None;
    thread::scope(|s| {
        let d = &d;
        let h = s.spawn(move || {
            let sink = ChanSink::new(tx);
            PBuilder::new(d, &sink, jobs, Exclude::none(dir)).build_dir(dir, ign)
        });
        for o in rx.iter() {
            objs.insert(o.key, o.bytes);
        }
        root = Some(h.join().expect("build thread").expect("parallel build"));
    });
    (root.unwrap(), objs)
}

fn assert_same_objects(want: &HashMap<Key, Vec<u8>>, got: &HashMap<Key, Vec<u8>>) {
    assert_eq!(want.len(), got.len(), "object count");
    for (k, wb) in want {
        match got.get(k) {
            None => panic!("missing object {k}"),
            Some(gb) => assert_eq!(wb, gb, "object {k} bytes differ"),
        }
    }
}

fn write_file(path: &Path, content: &[u8], mode: u32) {
    fs::write(path, content).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(mode)).unwrap();
}

/// Creates a deterministic, deep and wide directory tree: nested
/// subdirectories each holding several small files plus one large multi-chunk
/// file, so ingestion fans out across many files and subtrees (Go:
/// `writeDeepTree`).
fn write_deep_tree(root: &Path) {
    fn build(dir: &Path, depth: usize, seed: u64) {
        // A large file forces CDC into many chunks and a multi-level file
        // index.
        let mut large = vec![0u8; 256 << 10];
        fill_pseudo_random(&mut large, seed.wrapping_mul(1_000_003) + 7);
        write_file(&dir.join("large.bin"), &large, 0o644);
        // Several small files create multiple DirLeaf/DirNode objects.
        for i in 0..6 {
            let content = format!("depth={depth} seed={seed} index={i} payload");
            write_file(
                &dir.join(format!("file-{i:02}.txt")),
                content.as_bytes(),
                0o644,
            );
        }
        if depth == 0 {
            return;
        }
        for i in 0..3u64 {
            let sub = dir.join(format!("sub-{i}"));
            fs::create_dir(&sub).unwrap();
            build(&sub, depth - 1, seed * 10 + i + 1);
        }
    }
    build(root, 3, 1);
}

/// Fills `b` with a deterministic byte stream (a splitmix64-style generator)
/// so the large test files have enough entropy for content-defined chunking
/// to find boundaries, while remaining reproducible (Go:
/// `fillPseudoRandom`).
fn fill_pseudo_random(b: &mut [u8], seed: u64) {
    let mut x = seed;
    let mut i = 0;
    while i + 8 <= b.len() {
        x = x.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = x;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^= z >> 31;
        b[i..i + 8].copy_from_slice(&z.to_le_bytes());
        i += 8;
    }
}

/// Counts progress events; safe for concurrent use (Go: `countingProgress`).
#[derive(Default)]
struct CountingProgress {
    files: AtomicI64,
    bytes: AtomicI64,
}

impl Progress for CountingProgress {
    fn file_done(&self) {
        self.files.fetch_add(1, Ordering::SeqCst);
    }
    fn add_bytes(&self, n: usize) {
        self.bytes.fetch_add(n as i64, Ordering::SeqCst);
    }
}

// ---------------------------------------------------------------------------
// ingest_test.go
// ---------------------------------------------------------------------------

#[test]
fn objects_parity_with_sequential() {
    let tmp = TempDir::new().unwrap();
    let dir = tmp.path();
    write_file(&dir.join("a.txt"), b"alpha", 0o644);
    let sub = dir.join("sub");
    fs::create_dir(&sub).unwrap();
    write_file(&sub.join("b.txt"), b"beta", 0o644);

    let ic = ItemChunker::new(7);
    let (seq_root, seq_objs) = collect_sequential(dir, None, ic, None, 256);
    let (par_root, par_objs) = collect_parallel(dir, None, ic, None, 256, 4);
    assert_eq!(par_root, seq_root, "parallel root != sequential root");
    assert_same_objects(&seq_objs, &par_objs);
}

#[test]
fn objects_parallel_parity_deep_tree() {
    let tmp = TempDir::new().unwrap();
    write_deep_tree(tmp.path());
    let ic = ItemChunker::new(7);
    let (seq_root, seq_objs) = collect_sequential(tmp.path(), None, ic, None, 256);
    let jobs = thread::available_parallelism()
        .map_or(4, |n| n.get())
        .max(4);
    let (par_root, par_objs) = collect_parallel(tmp.path(), None, ic, None, 256, jobs);
    assert_eq!(par_root, seq_root, "parallel root != sequential root");
    assert!(
        seq_objs.len() >= 50,
        "deep tree produced only {} objects; expected a large fan-out",
        seq_objs.len()
    );
    assert_same_objects(&seq_objs, &par_objs);
}

/// Checks the public API: the stream yields every object, and the root is
/// set once the stream completes (Go: `TestObjects_RootSetAfterDrain`).
#[test]
fn objects_root_set_after_drain() {
    let tmp = TempDir::new().unwrap();
    write_file(&tmp.path().join("f"), b"data", 0o644);
    let (stream, root) = objects(
        tmp.path(),
        Opts {
            jobs: 2,
            ..Default::default()
        },
    )
    .unwrap();
    let mut seen = HashMap::new();
    for r in stream {
        let o = r.unwrap();
        seen.insert(o.key, true);
    }
    let root = root.get().expect("root not set after draining the stream");
    assert!(
        seen.contains_key(&root),
        "stream does not contain the root object {root}"
    );
    let (want_root, _) = collect_sequential(
        tmp.path(),
        None,
        ItemChunker::new(DEFAULT_ITEM_BITS),
        None,
        DEFAULT_XATTR_INLINE_MAX,
    );
    assert_eq!(root, want_root, "root != sequential oracle root");
}

/// Builds a single regular file and checks the root is a file-content key
/// (Blob or FileNode), not a directory (Go: `TestObjects_File`).
#[test]
fn objects_file() {
    let tmp = TempDir::new().unwrap();
    let f = tmp.path().join("f.txt");
    write_file(&f, b"hello world", 0o644);
    let (stream, root) = objects(&f, Opts::default()).unwrap();
    for r in stream {
        r.unwrap();
    }
    let typ = root.get().expect("root set").type_();
    assert!(
        typ == Type::Blob || typ == Type::FileNode,
        "file build root has type {typ:?}, want a file key"
    );
}

#[test]
fn objects_rejects_missing_path() {
    let tmp = TempDir::new().unwrap();
    let missing = tmp.path().join("does-not-exist");
    assert!(
        objects(&missing, Opts::default()).is_err(),
        "expected error building a missing path"
    );
}

/// Ingests into a real packstore and checks the stored root object
/// round-trips, and that the build is deterministic across worker counts
/// (Go: `TestDir_WritesToPackstore`).
#[test]
fn dir_writes_to_packstore() {
    let src = TempDir::new().unwrap();
    write_deep_tree(src.path());

    let mut roots = Vec::new();
    for jobs in [1usize, 8] {
        let store_dir = TempDir::new().unwrap();
        let st = packstore::Store::open(store_dir.path().join("packstore")).unwrap();
        let (stats, res) = dir(
            &st,
            src.path(),
            Opts {
                jobs,
                ..Default::default()
            },
        );
        let root = res.unwrap_or_else(|e| panic!("dir with {jobs} jobs: {e}"));
        assert!(stats.stored > 0, "dir with {jobs} jobs stored no objects");
        st.get(root)
            .unwrap_or_else(|e| panic!("root object not retrievable: {e}"));
        st.close().unwrap();
        roots.push(root);
    }
    assert_eq!(roots[0], roots[1], "root differs across jobs");
}

#[test]
fn objects_reports_progress() {
    let tmp = TempDir::new().unwrap();
    write_file(&tmp.path().join("a.txt"), b"alpha", 0o644); // 5
    write_file(&tmp.path().join("b.txt"), b"bravo!", 0o644); // 6
    let p = Arc::new(CountingProgress::default());
    let (stream, _root) = objects(
        tmp.path(),
        Opts {
            jobs: 2,
            progress: Some(p.clone()),
            ..Default::default()
        },
    )
    .unwrap();
    for r in stream {
        r.unwrap();
    }
    assert_eq!(p.bytes.load(Ordering::SeqCst), 11, "bytes");
    assert_eq!(p.files.load(Ordering::SeqCst), 2, "files");
}

// ---------------------------------------------------------------------------
// driver_test.go
// ---------------------------------------------------------------------------

#[test]
fn build_dir_fail_fast_on_unreadable_file() {
    if unsafe { libc::geteuid() } == 0 {
        eprintln!("skipping: running as root bypasses permission checks");
        return;
    }
    let tmp = TempDir::new().unwrap();
    write_file(&tmp.path().join("secret"), b"data", 0o000);
    let d = driver(ItemChunker::new(7), None, 256);
    let sink = MapSink::default();
    assert!(
        d.build_dir(tmp.path(), None, &sink).is_err(),
        "expected buildDir to fail on an unreadable file"
    );
}

// ---------------------------------------------------------------------------
// scan_test.go
// ---------------------------------------------------------------------------

#[test]
fn scan_tree_counts_regular_file_bytes() {
    let tmp = TempDir::new().unwrap();
    write_file(&tmp.path().join("a.txt"), b"alpha", 0o644); // 5
    write_file(&tmp.path().join("b.txt"), b"bravo!", 0o644); // 6
    let sub = tmp.path().join("sub");
    fs::create_dir(&sub).unwrap();
    write_file(&sub.join("c.txt"), b"cee", 0o644); // 3

    let (files, bytes) = scan_tree(tmp.path(), None, 4).unwrap();
    assert_eq!(files, 3, "files");
    assert_eq!(bytes, 14, "bytes");
}

#[test]
fn scan_tree_excludes_symlinks() {
    let tmp = TempDir::new().unwrap();
    write_file(&tmp.path().join("real"), b"1234", 0o644); // 4
    std::os::unix::fs::symlink("real", tmp.path().join("link")).unwrap();
    let (files, bytes) = scan_tree(tmp.path(), None, 2).unwrap();
    assert_eq!((files, bytes), (1, 4), "scan_tree");
}

#[test]
fn scan_tree_empty_dir() {
    let tmp = TempDir::new().unwrap();
    let (files, bytes) = scan_tree(tmp.path(), None, 4).unwrap();
    assert_eq!((files, bytes), (0, 0), "scan_tree(empty)");
}

// ---------------------------------------------------------------------------
// meta_test.go
// ---------------------------------------------------------------------------

#[test]
fn entry_meta_regular_file() {
    let tmp = TempDir::new().unwrap();
    let p = tmp.path().join("f");
    write_file(&p, b"hi", 0o640);
    let md = fs::symlink_metadata(&p).unwrap();
    let m = meta::entry_meta(&md);
    assert_eq!(m.mode & meta::S_IFMT, meta::S_IFREG, "type bits");
    assert_eq!(m.mode & 0o777, 0o640, "perm bits");
}

#[test]
fn read_xattrs_round_trip() {
    let tmp = TempDir::new().unwrap();
    let p = tmp.path().join("f");
    write_file(&p, b"x", 0o644);
    if let Err(e) = xattr::set(&p, "user.greeting", b"hello") {
        eprintln!("skipping: xattrs unsupported on this filesystem: {e}");
        return;
    }
    let m = super::xattrs::read_xattrs(&p).unwrap();
    assert_eq!(
        m.get(b"user.greeting".as_slice()).map(Vec::as_slice),
        Some(b"hello".as_slice()),
        "xattr value"
    );
}

// ---------------------------------------------------------------------------
// amberignore_test.go
// ---------------------------------------------------------------------------

/// Populates `dir` with a tree containing `.amberignore` files and entries
/// they exclude. With `pruned` the excluded entries are not written,
/// producing exactly the tree a filtered ingest of the full variant should
/// store (including the `.amberignore` files, which are always ingested)
/// (Go: `writeIgnoredTree`).
fn write_ignored_tree(dir: &Path, pruned: bool) {
    let write = |rel: &str, content: &str| {
        let p = dir.join(rel);
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        write_file(&p, content.as_bytes(), 0o644);
    };
    // Present in both variants.
    write(".amberignore", "*.log\nbuild/\n!keep.log\n");
    write("a.txt", "alpha");
    write("keep.log", "negated, kept");
    write("sub/.amberignore", "secret*\n");
    write("sub/ok.txt", "ok");
    write(
        "sub/build",
        "a file named build: dir-only pattern must not match",
    );
    write("sub/deeper/data.txt", "data");
    std::os::unix::fs::symlink("a.txt", dir.join("link.txt")).unwrap();
    if !pruned {
        // Excluded by the patterns above.
        write("app.log", "ignored by *.log");
        write("build/x.txt", "build/ prunes the whole directory");
        write(
            "old.log/inside.txt",
            "*.log without trailing slash matches dirs too",
        );
        write("sub/secret.txt", "ignored by sub/.amberignore");
        write(
            "sub/deeper/secret-2",
            "floating pattern applies in deeper subdirs",
        );
        std::os::unix::fs::symlink("a.txt", dir.join("link.log")).unwrap();
    }
    normalize_mtimes(dir);
}

/// Pins every entry's mtime to a fixed instant. Ingested entries carry their
/// lstat mtime, so the equal-root oracle (filtered full tree vs. separately
/// written pruned tree) only holds when both trees have identical
/// timestamps. Children are touched before their parent (reverse pre-order),
/// exactly as in Go (Go: `normalizeMtimes`).
fn normalize_mtimes(dir: &Path) {
    const FIXED_SECS: libc::time_t = 1_700_000_000;
    fn collect(dir: &Path, out: &mut Vec<PathBuf>) {
        out.push(dir.to_path_buf());
        if dir.symlink_metadata().unwrap().is_dir() {
            let mut ents: Vec<_> = fs::read_dir(dir)
                .unwrap()
                .map(|de| de.unwrap().path())
                .collect();
            ents.sort();
            for p in ents {
                collect(&p, out);
            }
        }
    }
    let mut paths = Vec::new();
    collect(dir, &mut paths);
    for p in paths.iter().rev() {
        let ts = libc::timespec {
            tv_sec: FIXED_SECS,
            tv_nsec: 0,
        };
        let c = CString::new(p.as_os_str().as_bytes()).unwrap();
        let rc = unsafe {
            // SAFETY: `c` is a valid NUL-terminated path and the times array
            // holds two initialized timespecs, as utimensat requires.
            libc::utimensat(
                libc::AT_FDCWD,
                c.as_ptr(),
                [ts, ts].as_ptr(),
                libc::AT_SYMLINK_NOFOLLOW,
            )
        };
        assert_eq!(
            rc,
            0,
            "utimensat {}: {}",
            p.display(),
            std::io::Error::last_os_error()
        );
    }
}

/// The matcher for `dir`'s own `.amberignore` (Go: `rootMatcher`).
fn root_matcher(dir: &Path) -> Matcher {
    Matcher::root(dir).unwrap()
}

/// Filtering the full tree must produce the exact same root as ingesting a
/// tree that never contained the ignored entries — files pruned, directories
/// not descended, `.amberignore` stored (Go:
/// `TestBuildDir_HonorsAmberignore`).
#[test]
fn build_dir_honors_amberignore() {
    let full = TempDir::new().unwrap();
    write_ignored_tree(full.path(), false);
    let pruned = TempDir::new().unwrap();
    write_ignored_tree(pruned.path(), true);

    let ic = ItemChunker::new(7);
    let (got_root, _) =
        collect_sequential(full.path(), Some(&root_matcher(full.path())), ic, None, 256);
    let (want_root, _) = collect_sequential(pruned.path(), None, ic, None, 256);
    assert_eq!(
        got_root, want_root,
        "filtered ingest root != pruned tree root"
    );
}

#[test]
fn ingest_objects_amberignore_parity() {
    let tmp = TempDir::new().unwrap();
    write_ignored_tree(tmp.path(), false);
    let ic = ItemChunker::new(7);
    let (seq_root, seq_objs) =
        collect_sequential(tmp.path(), Some(&root_matcher(tmp.path())), ic, None, 256);
    let (par_root, par_objs) = collect_parallel(
        tmp.path(),
        Some(&root_matcher(tmp.path())),
        ic,
        None,
        256,
        4,
    );
    assert_eq!(par_root, seq_root, "parallel root != sequential root");
    assert_same_objects(&seq_objs, &par_objs);
}

#[test]
fn build_dir_nil_matcher_ingests_everything() {
    let full = TempDir::new().unwrap();
    write_ignored_tree(full.path(), false);
    let pruned = TempDir::new().unwrap();
    write_ignored_tree(pruned.path(), true);

    let ic = ItemChunker::new(7);
    let (full_root, _) = collect_sequential(full.path(), None, ic, None, 256);
    let (pruned_root, _) = collect_sequential(pruned.path(), None, ic, None, 256);
    assert_ne!(
        full_root, pruned_root,
        "nil matcher must ingest the ignored entries"
    );
}

/// The pre-scan must count exactly the entries the filtered ingest will read
/// (Go: `TestScanTree_HonorsAmberignore`).
#[test]
fn scan_tree_honors_amberignore() {
    let full = TempDir::new().unwrap();
    write_ignored_tree(full.path(), false);
    let pruned = TempDir::new().unwrap();
    write_ignored_tree(pruned.path(), true);

    let got = scan_tree(full.path(), Some(&root_matcher(full.path())), 4).unwrap();
    let want = scan_tree(pruned.path(), None, 4).unwrap();
    assert_eq!(got, want, "scan_tree (files, bytes)");
}

/// End-to-end consistency — the scan's totals equal what the filtered build
/// actually processes (Go: `TestProgressTotalsMatchIngestWithAmberignore`).
#[test]
fn progress_totals_match_ingest_with_amberignore() {
    let tmp = TempDir::new().unwrap();
    write_ignored_tree(tmp.path(), false);
    let (files, bytes) = scan(tmp.path(), false, 4).unwrap();
    let p = Arc::new(CountingProgress::default());
    let (stream, _root) = objects(
        tmp.path(),
        Opts {
            jobs: 2,
            progress: Some(p.clone()),
            ..Default::default()
        },
    )
    .unwrap();
    for r in stream {
        r.unwrap();
    }
    assert_eq!(p.files.load(Ordering::SeqCst) as u64, files, "files done");
    assert_eq!(p.bytes.load(Ordering::SeqCst) as u64, bytes, "bytes done");
}

/// Drains a public-API build of `path` and returns its root (Go:
/// `objectsRoot`).
fn objects_root(path: &Path, opts: Opts) -> Key {
    let (stream, root) = objects(path, opts).unwrap();
    for r in stream {
        r.unwrap();
    }
    root.get().expect("root set")
}

#[test]
fn objects_honors_amberignore() {
    let full = TempDir::new().unwrap();
    write_ignored_tree(full.path(), false);
    let pruned = TempDir::new().unwrap();
    write_ignored_tree(pruned.path(), true);
    assert_eq!(
        objects_root(full.path(), Opts::default()),
        objects_root(pruned.path(), Opts::default()),
        "filtered build root != pruned tree root"
    );
}

#[test]
fn objects_no_ignore() {
    let full = TempDir::new().unwrap();
    write_ignored_tree(full.path(), false);
    let with_ignore = objects_root(full.path(), Opts::default());
    let without_ignore = objects_root(
        full.path(),
        Opts {
            no_ignore: true,
            ..Default::default()
        },
    );
    assert_ne!(
        with_ignore, without_ignore,
        "no_ignore must include the ignored entries"
    );
}

// ---------------------------------------------------------------------------
// decode_test.go
// ---------------------------------------------------------------------------

/// The decoded object set: key → object bytes (Go: `store`).
type Store = HashMap<Key, Vec<u8>>;

/// Reassembles a file's bytes from its content key (Go: `store.fileContent`).
fn file_content(s: &Store, k: Key) -> Vec<u8> {
    match k.type_() {
        Type::Blob => s[&k].clone(),
        Type::FileNode => {
            let children = decode_file_node(&s[&k]).unwrap();
            let mut out = Vec::new();
            for ck in children {
                out.extend_from_slice(&file_content(s, ck));
            }
            out
        }
        other => panic!("not a file content key: {other:?}"),
    }
}

/// Collects all entries under a directory content key, descending DirNodes
/// and DirLeaves (Go: `store.listDir`).
fn list_dir(s: &Store, k: Key) -> Vec<Entry> {
    match k.type_() {
        Type::DirLeaf => decode_dir_leaf(&s[&k]).unwrap(),
        Type::DirNode => {
            let pairs = decode_dir_node(&s[&k]).unwrap();
            let mut out = Vec::new();
            for p in pairs {
                let ck = Key::parse(&p.child_key).unwrap();
                out.extend(list_dir(s, ck));
            }
            out
        }
        other => panic!("not a directory key: {other:?}"),
    }
}

#[test]
fn end_to_end_structure_round_trips() {
    let tmp = TempDir::new().unwrap();
    let dir = tmp.path();
    let big: Vec<u8> = b"0123456789".repeat(200_000); // ~2 MiB -> multi-chunk file
    write_file(&dir.join("big.bin"), &big, 0o644);
    write_file(&dir.join("small.txt"), b"tiny", 0o600);
    std::os::unix::fs::symlink("big.bin", dir.join("link")).unwrap();

    let (root, s) = collect_sequential(dir, None, ItemChunker::new(7), None, 256);

    let ents = list_dir(&s, root);
    let by_name: HashMap<Vec<u8>, Entry> = ents.into_iter().map(|e| (e.name.clone(), e)).collect();

    let big_entry = by_name.get(b"big.bin".as_slice()).expect("big.bin missing");
    let big_key = Key::parse(&big_entry.content_key).unwrap();
    let got = file_content(&s, big_key);
    assert_eq!(got.len(), big.len(), "big.bin content length mismatch");
    assert_eq!(got, big, "big.bin content mismatch");
    assert_eq!(
        big_key.length(),
        big.len() as u64,
        "big.bin content key length"
    );

    let small = &by_name[b"small.txt".as_slice()];
    assert_eq!(small.mode & 0o777, 0o600, "small.txt perms");

    let link = &by_name[b"link".as_slice()];
    assert_eq!(link.mode & meta::S_IFMT, meta::S_IFLNK, "link type");
    assert_eq!(link.link_target, b"big.bin", "link target");
}

// ---------------------------------------------------------------------------
// exclude_test.go
// ---------------------------------------------------------------------------

/// A tree with a metadata dir at the root and a same-named dir one level
/// down, which must not be excluded (Go: `excludeFixture`).
fn exclude_fixture() -> TempDir {
    let tmp = TempDir::new().unwrap();
    let dir = tmp.path();
    write_file(&dir.join("a.txt"), b"alpha", 0o644);
    fs::create_dir_all(dir.join(".meta")).unwrap();
    write_file(&dir.join(".meta").join("junk"), &[0u8; 1000], 0o644);
    fs::create_dir_all(dir.join("sub").join(".meta")).unwrap();
    write_file(&dir.join("sub").join(".meta").join("keep"), b"keep", 0o644);
    tmp
}

/// An unsynced packstore in its own temp dir (Go: `packstore.Open(...,
/// packstore.WithSync(false))`).
fn open_unsynced_store() -> (TempDir, packstore::Store) {
    let store_dir = TempDir::new().unwrap();
    let st = packstore::Store::open_with(
        store_dir.path().join("ps"),
        packstore::Options::new().sync(false),
    )
    .unwrap();
    (store_dir, st)
}

/// The excluded root name gives the same root as a tree that never had it
/// (Go: `TestExclude_SkipsRootNameOnly`).
#[test]
fn exclude_skips_root_name_only() {
    let tmp = exclude_fixture();
    let (_store_dir, st) = open_unsynced_store();
    let (_, res) = dir(
        &st,
        tmp.path(),
        Opts {
            exclude: vec![".meta".into()],
            ..Default::default()
        },
    );
    let got = res.unwrap();
    // The same tree without the root .meta must give the same key.
    fs::remove_dir_all(tmp.path().join(".meta")).unwrap();
    let (_, res) = dir(&st, tmp.path(), Opts::default());
    let want = res.unwrap();
    assert_eq!(got, want, "root with exclude != root without .meta");
    st.close().unwrap();
}

/// Exclude applies whatever `no_ignore` says (Go:
/// `TestExclude_IgnoresNoIgnore`).
#[test]
fn exclude_ignores_no_ignore() {
    let tmp = exclude_fixture();
    let (_store_dir, st) = open_unsynced_store();
    let (_, res) = dir(
        &st,
        tmp.path(),
        Opts {
            exclude: vec![".meta".into()],
            no_ignore: true,
            ..Default::default()
        },
    );
    let got = res.unwrap();
    fs::remove_dir_all(tmp.path().join(".meta")).unwrap();
    let (_, res) = dir(
        &st,
        tmp.path(),
        Opts {
            no_ignore: true,
            ..Default::default()
        },
    );
    let want = res.unwrap();
    assert_eq!(got, want, "exclude must apply with no_ignore");
    st.close().unwrap();
}

/// The sizing scan skips the excluded root name too (Go:
/// `TestScanWith_HonorsExclude`).
#[test]
fn scan_with_honors_exclude() {
    let tmp = exclude_fixture();
    let (files, bytes) = scan_with(
        tmp.path(),
        &Opts {
            exclude: vec![".meta".into()],
            ..Default::default()
        },
    )
    .unwrap();
    // a.txt (5 bytes) and sub/.meta/keep (4 bytes); the root .meta/junk is
    // skipped.
    assert_eq!((files, bytes), (2, 9), "files, bytes");
}
