//! Ported Go gc tests (`collector_test.go`, `cycle_test.go`,
//! `oracle_test.go`), plus Rust-only pins for the module's helpers.

use std::collections::HashMap;
use std::fs::{self, OpenOptions};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use tempfile::TempDir;

use crate::fstree::{encode_blob, encode_file_node};
use crate::key::Key;
use crate::packstore;
use crate::reference::Reference;
use crate::refstore;

use super::{Collector, Options, free_below, lock, throttle_owed};

pub(super) const HOUR: Duration = Duration::from_secs(60 * 60);

// ---------------------------------------------------------------------------
// Go math/rand/v2 PCG (test-only; go-spec §5.4). The oracle and the tree
// fixtures must consume the exact draw sequence the Go tests consume, so
// this ports Go's 128-bit LCG advance with DXSM output on the post-advance
// state. The bench port carries its own copy; unify if a third user appears
// (see port-notes/gc.md).

/// Go's `rand.PCG`: `NewPCG(seed1, seed2)` sets the 128-bit state directly.
struct Pcg {
    hi: u64,
    lo: u64,
}

impl Pcg {
    fn new(seed1: u64, seed2: u64) -> Pcg {
        Pcg {
            hi: seed1,
            lo: seed2,
        }
    }

    /// The 128-bit LCG state advance; returns the new state (Go:
    /// `(*PCG).next`).
    fn next(&mut self) -> (u64, u64) {
        const MUL_HI: u64 = 2549297995355413924;
        const MUL_LO: u64 = 4865540595714422341;
        const INC_HI: u64 = 6364136223846793005;
        const INC_LO: u64 = 1442695040888963407;
        let wide = u128::from(self.lo) * u128::from(MUL_LO);
        let mut hi = (wide >> 64) as u64;
        let lo = wide as u64;
        hi = hi
            .wrapping_add(self.hi.wrapping_mul(MUL_LO))
            .wrapping_add(self.lo.wrapping_mul(MUL_HI));
        let (lo, carry) = lo.overflowing_add(INC_LO);
        let hi = hi.wrapping_add(INC_HI).wrapping_add(u64::from(carry));
        self.lo = lo;
        self.hi = hi;
        (hi, lo)
    }

    /// PCG-DXSM output computed from the new state (Go: `(*PCG).Uint64`;
    /// numpy's pcg64dxsm outputs from the pre-advance state instead — do
    /// not substitute).
    fn uint64(&mut self) -> u64 {
        const CHEAP_MUL: u64 = 0xda94_2042_e4dd_58b5;
        let (mut hi, lo) = self.next();
        hi ^= hi >> 32;
        hi = hi.wrapping_mul(CHEAP_MUL);
        hi ^= hi >> 48;
        hi = hi.wrapping_mul(lo | 1);
        hi
    }

    /// Uniform draw below `n` (Go: `(*Rand).uint64n` on a 64-bit target —
    /// single masked draw for powers of two, Lemire's method otherwise).
    fn uint_n(&mut self, n: u64) -> u64 {
        if n.is_power_of_two() {
            return self.uint64() & (n - 1);
        }
        let mut wide = u128::from(self.uint64()) * u128::from(n);
        let mut lo = wide as u64;
        if lo < n {
            let thresh = n.wrapping_neg() % n;
            while lo < thresh {
                wide = u128::from(self.uint64()) * u128::from(n);
                lo = wide as u64;
            }
        }
        (wide >> 64) as u64
    }

    /// Go: `(*Rand).IntN`.
    fn int_n(&mut self, n: usize) -> usize {
        self.uint_n(n as u64) as usize
    }
}

/// CRC-32/IEEE (polynomial 0xEDB88320), bitwise — the `crc32c` crate is
/// Castagnoli-only and `storeTree` seeds come from Go's
/// `crc32.ChecksumIEEE`.
fn crc32_ieee(data: &[u8]) -> u32 {
    let mut crc = !0u32;
    for &b in data {
        crc ^= u32::from(b);
        for _ in 0..8 {
            crc = if crc & 1 != 0 {
                (crc >> 1) ^ 0xEDB8_8320
            } else {
                crc >> 1
            };
        }
    }
    !crc
}

/// Pins the PCG port to draws produced by Go 1.26 `math/rand/v2` with the
/// exact seeds the ported tests use (values generated from Go; see
/// port-notes/gc.md).
#[test]
fn pcg_matches_go_reference_draws() {
    let mut r = Pcg::new(7, 11);
    for want in [
        4272534329212569458u64,
        7249340062685939787,
        9598922724696430034,
        3291815920136860081,
        6184812529643652431,
    ] {
        assert_eq!(r.uint64(), want);
    }
    let mut r = Pcg::new(7, 11);
    for want in [0usize, 1, 1, 0, 1, 0, 1, 0] {
        assert_eq!(r.int_n(3), want);
    }
    let mut r = Pcg::new(7, 11);
    for want in [4usize, 7, 10, 3, 6, 5, 13, 4] {
        assert_eq!(r.int_n(20), want);
    }
    assert_eq!(crc32_ieee(b"keep"), 3421521931);
    let mut r = Pcg::new(u64::from(crc32_ieee(b"keep")), 0);
    for want in [249u64, 221, 109, 149, 43, 190, 120, 152] {
        assert_eq!(r.uint_n(256), want);
    }
}

// ---------------------------------------------------------------------------
// Test fixtures (Go: collector_test.go helpers).

/// An open packstore+refstore pair in one temp dir (Go: `testStore`; the
/// stores drop-close after any collector, whose `Drop` closes it first).
pub(super) struct TestStore {
    pub(super) dir: PathBuf,
    pub(super) objects: Arc<packstore::Store>,
    pub(super) refs: Arc<refstore::Store>,
    _tmp: TempDir,
}

/// Go: `newTestStore`. Packstore sync stays at its default (true), as in Go.
pub(super) fn new_test_store(seg_size: u64) -> TestStore {
    let tmp = TempDir::new().unwrap();
    let dir = tmp.path().to_path_buf();
    let objects = Arc::new(
        packstore::Store::open_with(
            dir.join("packstore"),
            packstore::Options::new().segment_size(seg_size),
        )
        .unwrap(),
    );
    let refs = Arc::new(refstore::Store::open(dir.join("refs"), true).unwrap());
    TestStore {
        dir,
        objects,
        refs,
        _tmp: tmp,
    }
}

/// Go: `(*testStore).openCollector`.
pub(super) fn open_collector(ts: &TestStore, opts: Options) -> Collector {
    Collector::open(
        ts.dir.join("closures"),
        Arc::clone(&ts.objects),
        Arc::clone(&ts.refs),
        opts,
    )
    .unwrap()
}

/// Stores a FileNode root over `n` distinct incompressible 256-byte blobs
/// derived from `seed` and returns the root and every key. Incompressible
/// payloads keep on-disk sizes predictable so small segment sizes actually
/// rotate (Go: `storeTree`).
pub(super) fn store_tree(objects: &packstore::Store, seed: &str, n: usize) -> (Key, Vec<Key>) {
    let base = u64::from(crc32_ieee(seed.as_bytes()));
    let mut children = Vec::new();
    let mut all = Vec::new();
    for i in 0..n {
        let mut rng = Pcg::new(base, i as u64);
        let data: Vec<u8> = (0..256).map(|_| rng.uint_n(256) as u8).collect();
        let o = encode_blob(&data);
        objects.put(o.key, &o.bytes).unwrap();
        children.push(o.key);
        all.push(o.key);
    }
    let root = encode_file_node(&children);
    objects.put(root.key, &root.bytes).unwrap();
    all.push(root.key);
    (root.key, all)
}

/// Writes a reference through the collector exactly as a CLI/daemon PUT
/// does (Go: `putTestRef`).
pub(super) fn put_test_ref(c: &Collector, refs: &refstore::Store, name: &str, root: Key) {
    let rec = Reference {
        name: name.to_string(),
        key: root.as_bytes().to_vec(),
        user: String::new(),
        created_at: SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos() as i64,
        signature: Vec::new(),
        public_key: Vec::new(),
    };
    let raw = rec.encode().unwrap();
    let old = match refs.get(name) {
        Ok(prev) => {
            let prev_ref = Reference::decode(&prev).unwrap();
            Some(Key::parse(&prev_ref.key).unwrap())
        }
        Err(e) if e.is_not_found() => None,
        Err(e) => panic!("{e}"),
    };
    let prepared = c.prepare_ref(root).unwrap();
    match refs.put(name, &raw) {
        Ok(()) => prepared.commit(),
        Err(e) => {
            prepared.abort();
            panic!("{e}");
        }
    }
    if let Some(old) = old {
        c.release_ref(old).unwrap();
    }
}

/// Go: `rmTestRef`.
pub(super) fn rm_test_ref(c: &Collector, refs: &refstore::Store, name: &str, root: Key) {
    refs.delete(name).unwrap();
    c.release_ref(root).unwrap();
}

/// Pushes every sealed pack's mtime behind any grace period (Go:
/// `backdatePacks`, via `os.Chtimes`).
pub(super) fn backdate_packs(ts: &TestStore) {
    let dir = ts.dir.join("packstore");
    let old = SystemTime::now() - Duration::from_secs(2 * 60 * 60);
    for entry in fs::read_dir(&dir).unwrap() {
        let entry = entry.unwrap();
        if entry.file_name().to_string_lossy().ends_with(".seg") {
            let f = OpenOptions::new().write(true).open(entry.path()).unwrap();
            f.set_modified(old).unwrap();
        }
    }
}

// ---------------------------------------------------------------------------
// Collector tests (Go: collector_test.go).

#[test]
fn prepare_ref_missing_object_fails() {
    let ts = new_test_store(1 << 20);
    let c = open_collector(&ts, Options::default());
    // A root whose child blob was never stored: the completeness walk is
    // the caller's 404.
    let blob = encode_blob(b"never stored");
    let root = encode_file_node(&[blob.key]);
    ts.objects.put(root.key, &root.bytes).unwrap();
    let err = c
        .prepare_ref(root.key)
        .expect_err("prepare_ref accepted a root with a missing object");
    assert!(
        err.to_string().starts_with("gc: walking root "),
        "err = {err}"
    );
}

#[test]
fn open_sweeps_stale_closure_state() {
    let ts = new_test_store(1 << 20);
    let dir = ts.dir.join("closures");
    fs::create_dir_all(dir.join("tmp")).unwrap();
    fs::write(dir.join("deadbeef.tails"), b"old").unwrap();
    let _c = open_collector(&ts, Options::default());
    let ents: Vec<_> = fs::read_dir(&dir)
        .unwrap()
        .map(|e| e.unwrap().file_name())
        .collect();
    assert!(
        ents.is_empty(),
        "stale closure state survived open: {ents:?}"
    );
}

#[test]
fn status_scores_and_counts() {
    let ts = new_test_store(4 << 10);
    let c = open_collector(&ts, Options::default());
    let (root_a, _keys_a) = store_tree(&ts.objects, "a", 30);
    let (root_b, _keys_b) = store_tree(&ts.objects, "b", 30);
    put_test_ref(&c, &ts.refs, "a", root_a);
    put_test_ref(&c, &ts.refs, "b", root_b);

    let st = c.status().unwrap();
    assert_eq!(st.refs, 2, "refs");
    assert!(st.marked > 0, "marked = 0");
    assert_eq!(st.garbage_bytes, 0, "garbage_bytes before any delete");

    rm_test_ref(&c, &ts.refs, "b", root_b);
    let st = c.status().unwrap();
    assert_eq!(st.refs, 1, "refs after delete");
    assert!(
        st.garbage_bytes > 0,
        "garbage_bytes = 0 after deleting a ref with unique data"
    );
}

#[test]
fn why_names_the_holding_ref() {
    let ts = new_test_store(1 << 20);
    let c = open_collector(&ts, Options::default());
    let (root_a, keys_a) = store_tree(&ts.objects, "a", 4);
    let (root_b, _) = store_tree(&ts.objects, "b", 4);
    put_test_ref(&c, &ts.refs, "va", root_a);
    put_test_ref(&c, &ts.refs, "vb", root_b);

    let names = c.why(keys_a[0]).unwrap();
    assert_eq!(names, ["va"], "why");
    rm_test_ref(&c, &ts.refs, "va", root_a);
    let names = c.why(keys_a[0]).unwrap();
    assert!(names.is_empty(), "why after rm = {names:?}, want none");
}

// ---------------------------------------------------------------------------
// Cycle tests (Go: cycle_test.go).

#[test]
fn run_reaps_dead_keeps_live() {
    let ts = new_test_store(4 << 10);
    let c = open_collector(
        &ts,
        Options {
            grace: HOUR,
            ..Options::default()
        },
    );
    let (root_keep, keys_keep) = store_tree(&ts.objects, "keep", 40);
    let (root_dead, keys_dead) = store_tree(&ts.objects, "dead", 40);
    put_test_ref(&c, &ts.refs, "keep", root_keep);
    put_test_ref(&c, &ts.refs, "dead", root_dead);
    rm_test_ref(&c, &ts.refs, "dead", root_dead);

    backdate_packs(&ts);
    let stats = c.run(0.0).unwrap();
    assert!(!stats.reaped.is_empty(), "nothing reaped: {stats:?}");
    assert!(stats.marked > 0, "marked = 0");
    for k in &keys_keep {
        if let Err(e) = ts.objects.get(*k) {
            panic!("live key {k}: {e}");
        }
    }
    let gone = keys_dead
        .iter()
        .filter(|k| matches!(ts.objects.get(**k), Err(ref e) if e.is_not_found()))
        .count();
    assert!(gone > 0, "no dead key was collected");
}

#[test]
fn run_respects_grace() {
    let ts = new_test_store(4 << 10);
    let c = open_collector(
        &ts,
        Options {
            grace: HOUR,
            ..Options::default()
        },
    );
    let (root, keys) = store_tree(&ts.objects, "young", 40);
    put_test_ref(&c, &ts.refs, "v", root);
    rm_test_ref(&c, &ts.refs, "v", root);

    // No backdate: every pack is younger than the grace period.
    let stats = c.run(0.0).unwrap();
    assert!(stats.reaped.is_empty(), "reaped young packs: {stats:?}");
    for k in &keys {
        if let Err(e) = ts.objects.get(*k) {
            panic!("young unreferenced key {k} collected: {e}");
        }
    }
}

/// A reference committed between the mark and the sweep names a
/// pre-existing, unmarked tree; its walked closure joins the grey set, so
/// the sweep keeps it while still collecting other garbage of the same
/// vintage (Go: `TestRefPutDuringMark`, via the `midMark` hook).
#[test]
fn ref_put_during_mark_keeps_late_closure() {
    let ts = new_test_store(4 << 10);
    let c = open_collector(
        &ts,
        Options {
            grace: HOUR,
            ..Options::default()
        },
    );
    let (root_late, keys_late) = store_tree(&ts.objects, "late", 40);
    let (_root_dead, keys_dead) = store_tree(&ts.objects, "dead", 40);
    let (root_keep, _) = store_tree(&ts.objects, "keep", 4);
    put_test_ref(&c, &ts.refs, "keep", root_keep);
    backdate_packs(&ts);

    let hook_c = c.test_handle();
    let hook_refs = Arc::clone(&ts.refs);
    lock(&c.core.mu).mid_mark = Some(Arc::new(move || {
        put_test_ref(&hook_c, &hook_refs, "late", root_late);
    }));
    let stats = c.run(0.0).unwrap();
    // Break the hook → handle → core Arc cycle before the test ends. The
    // hook must be dropped *outside* the `mu` lock: it owns a collector
    // handle whose close() takes `mu` again.
    let hook = lock(&c.core.mu).mid_mark.take();
    drop(hook);

    assert!(!stats.reaped.is_empty(), "nothing reaped: {stats:?}");
    for k in &keys_late {
        if let Err(e) = ts.objects.get(*k) {
            panic!("late-referenced key {k} lost to the sweep: {e}");
        }
    }
    let gone = keys_dead
        .iter()
        .filter(|k| matches!(ts.objects.get(**k), Err(ref e) if e.is_not_found()))
        .count();
    assert!(
        gone > 0,
        "the sweep collected nothing else, so the test proves nothing"
    );
}

#[test]
fn run_overlap_refused() {
    let ts = new_test_store(1 << 20);
    let c = open_collector(&ts, Options::default());
    let _cycle = lock(&c.core.cycle_mu);
    let err = c.run(0.0).expect_err("overlapping run accepted");
    assert!(err.is_cycle_running(), "err = {err}, want CycleRunning");
}

// ---------------------------------------------------------------------------
// Oracle test (Go: oracle_test.go): random reference churn and cycles
// against an in-memory model of what must stay readable.

#[test]
fn oracle_random_churn() {
    let ts = new_test_store(4 << 10);
    let c = open_collector(
        &ts,
        Options {
            grace: HOUR,
            ..Options::default()
        },
    );
    let mut rng = Pcg::new(7, 11);

    struct Tree {
        root: Key,
        keys: Vec<Key>,
    }
    let mut live: HashMap<String, Tree> = HashMap::new();
    let mut graveyard: Vec<Tree> = Vec::new(); // unreferenced; unique data may vanish

    let ref_names = ["a", "b", "c", "d"];
    for round in 0..30 {
        let name = ref_names[rng.int_n(ref_names.len())];
        match rng.int_n(3) {
            0 => {
                // Set to a fresh tree.
                let n = rng.int_n(20) + 5;
                let (root, keys) = store_tree(&ts.objects, &format!("t{round}-"), n);
                put_test_ref(&c, &ts.refs, name, root);
                if let Some(old) = live.insert(name.to_string(), Tree { root, keys }) {
                    graveyard.push(old);
                }
            }
            1 => {
                // Delete.
                if let Some(old) = live.remove(name) {
                    ts.refs.delete(name).unwrap();
                    c.release_ref(old.root).unwrap();
                    graveyard.push(old);
                }
            }
            2 => {
                // Cycle at a random line.
                backdate_packs(&ts);
                let line = [-1.0, 0.0, 0.3][rng.int_n(3)];
                if let Err(e) = c.run(line) {
                    panic!("round {round}: run: {e}");
                }
            }
            _ => unreachable!(),
        }
        // Invariant: everything referenced reads back, always.
        for (name, tree) in &live {
            for k in &tree.keys {
                if let Err(e) = ts.objects.get(*k) {
                    panic!("round {round}: ref {name:?} key {k}: {e}");
                }
            }
        }
    }
    // Force a full sweep and re-verify, then reopen the collector and
    // verify a fresh mark reaches the same conclusion.
    backdate_packs(&ts);
    c.run(0.0).unwrap();
    for tree in live.values() {
        for k in &tree.keys {
            if let Err(e) = ts.objects.get(*k) {
                panic!("after sweep: {e}");
            }
        }
    }
    c.close().unwrap();
    drop(c);
    let c2 = open_collector(
        &ts,
        Options {
            grace: HOUR,
            ..Options::default()
        },
    );
    backdate_packs(&ts);
    c2.run(0.0).unwrap();
    for tree in live.values() {
        for k in &tree.keys {
            if let Err(e) = ts.objects.get(*k) {
                panic!("after reopen sweep: {e}");
            }
        }
    }
    let _ = graveyard; // dead trees: no assertion — they may or may not be gone yet
}

// ---------------------------------------------------------------------------
// Rust-only unit pins (no Go counterpart; they pin the gc.go helpers the Go
// suite leaves untested).

#[test]
fn throttle_owed_divides_before_multiplying() {
    assert_eq!(throttle_owed(1, 1), Duration::from_secs(1));
    assert_eq!(throttle_owed(1, 2), Duration::from_millis(500));
    // 10 GiB at 100 MiB/s: the naive n*1e9 nanoseconds would overflow i64.
    assert_eq!(
        throttle_owed(10 << 30, 100 << 20),
        Duration::from_millis(102_400)
    );
}

#[test]
fn free_below_probes_the_filesystem() {
    let dir = TempDir::new().unwrap();
    // No filesystem has less than one byte free, and none this test runs on
    // has 2^64 bytes free.
    assert!(!free_below(dir.path(), 1));
    assert!(free_below(dir.path(), u64::MAX));
    // A failed probe must report no pressure.
    assert!(!free_below(&dir.path().join("missing"), u64::MAX));
}

// Wipe must not reset the stores while a cycle is still marking (Go:
// TestWipeWaitsForRunningCycleBeforeReset).
#[test]
fn wipe_waits_for_running_cycle_before_reset() {
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::thread;

    let ts = new_test_store(4 << 10);
    let c = open_collector(&ts, Options::default());
    let (root, _) = store_tree(&ts.objects, "a", 10);
    put_test_ref(&c, &ts.refs, "a", root);

    let entered = Arc::new(AtomicBool::new(false));
    let release = Arc::new(AtomicBool::new(false));
    {
        let (entered, release) = (Arc::clone(&entered), Arc::clone(&release));
        lock(&c.core.mu).mid_mark = Some(Arc::new(move || {
            entered.store(true, Ordering::SeqCst);
            while !release.load(Ordering::SeqCst) {
                thread::sleep(Duration::from_millis(1));
            }
        }));
    }
    let run_c = c.test_handle();
    let cycle = thread::spawn(move || {
        let _ = run_c.run(0.0); // canceled by wipe
    });
    while !entered.load(Ordering::SeqCst) {
        thread::sleep(Duration::from_millis(1));
    }

    let reset_called = Arc::new(AtomicBool::new(false));
    let wipe_c = c.test_handle();
    let wipe = {
        let reset_called = Arc::clone(&reset_called);
        thread::spawn(move || {
            wipe_c.wipe(|| {
                reset_called.store(true, Ordering::SeqCst);
                Ok::<(), std::convert::Infallible>(())
            })
        })
    };
    thread::sleep(Duration::from_millis(50));
    assert!(
        !reset_called.load(Ordering::SeqCst),
        "reset ran before the cycle finished"
    );
    release.store(true, Ordering::SeqCst);
    wipe.join().unwrap().expect("wipe");
    assert!(reset_called.load(Ordering::SeqCst));
    cycle.join().unwrap();
    let hook = lock(&c.core.mu).mid_mark.take();
    drop(hook);
}

// ---------------------------------------------------------------------------
// Commit history (Go: commit_test.go). No production change backs these: the
// mark, prepare_ref and why all dispatch through fstree::child_keys.

/// Stores `store_tree`'s file under a one-entry directory and returns the
/// directory root (a commit's tree must be a directory) and every key.
fn store_dir_tree(objects: &packstore::Store, seed: &str, n: usize) -> (Key, Vec<Key>) {
    let (file, mut all) = store_tree(objects, seed, n);
    let leaf = crate::fstree::encode_dir_leaf(&[crate::fstree::Entry {
        name: b"f".to_vec(),
        mode: 0o100644,
        content_key: file.as_bytes().to_vec(),
        ..Default::default()
    }])
    .unwrap();
    objects.put(leaf.key, &leaf.bytes).unwrap();
    all.push(leaf.key);
    (leaf.key, all)
}

/// Builds a commit of `tree` without storing it.
fn new_commit(tree: Key, parents: &[Key]) -> (Key, Vec<u8>) {
    let id = crate::commit::Identity {
        name: "Ann".into(),
        when: 1,
        ..Default::default()
    };
    crate::commit::Commit {
        tree,
        parents: parents.to_vec(),
        author: id.clone(),
        committer: id,
        message: "m".into(),
        signature: Vec::new(),
        public_key: Vec::new(),
        change_id: Vec::new(),
        conflict_terms: Vec::new(),
        conflict_labels: Vec::new(),
    }
    .object()
    .unwrap()
}

fn store_commit(objects: &packstore::Store, tree: Key, parents: &[Key]) -> Key {
    let (k, b) = new_commit(tree, parents);
    objects.put(k, &b).unwrap();
    k
}

pub(super) fn count_gone(objects: &packstore::Store, keys: &[Key]) -> usize {
    keys.iter()
        .filter(|k| matches!(objects.get(**k), Err(ref e) if e.is_not_found()))
        .count()
}

#[test]
fn commit_history_stays_live() {
    let ts = new_test_store(4 << 10);
    let c = open_collector(
        &ts,
        Options {
            grace: HOUR,
            ..Options::default()
        },
    );
    let (tree_old, keys_old) = store_dir_tree(&ts.objects, "old", 40);
    let (tree_new, keys_new) = store_dir_tree(&ts.objects, "new", 40);
    let (_, keys_dead) = store_dir_tree(&ts.objects, "dead", 40); // never referenced
    let first = store_commit(&ts.objects, tree_old, &[]);
    let tip = store_commit(&ts.objects, tree_new, &[first]);
    put_test_ref(&c, &ts.refs, "main", tip);

    // why walks through the commits: the old tree is held by the branch.
    assert_eq!(c.why(keys_old[0]).unwrap(), ["main"], "why(old blob)");

    backdate_packs(&ts);
    let stats = c.run(0.0).unwrap();
    assert!(!stats.reaped.is_empty(), "nothing reaped: {stats:?}");
    for k in keys_old.iter().chain(&keys_new).chain(&[first, tip]) {
        if let Err(e) = ts.objects.get(*k) {
            panic!("key {k} reachable from the branch: {e}");
        }
    }
    assert!(
        count_gone(&ts.objects, &keys_dead) > 0,
        "no unreferenced key was collected"
    );

    // Dropping the branch makes the whole history garbage.
    rm_test_ref(&c, &ts.refs, "main", tip);
    backdate_packs(&ts);
    c.run(0.0).unwrap();
    assert!(
        count_gone(&ts.objects, &keys_old) > 0,
        "no key of the old commit's tree was collected after the branch went"
    );
}

/// A plain directory may hold a commit as the content key of a directory
/// entry. Whatever keeps that directory alive keeps the commit, its tree and
/// its ancestors alive; the walks follow content keys whatever their type
/// (Go: `TestTreeHoldingACommitKeepsItsHistoryLive`).
#[test]
fn tree_holding_a_commit_keeps_its_history_live() {
    let ts = new_test_store(4 << 10);
    let c = open_collector(
        &ts,
        Options {
            grace: HOUR,
            ..Options::default()
        },
    );
    let (tree_old, keys_old) = store_dir_tree(&ts.objects, "old", 40);
    let (tree_new, keys_new) = store_dir_tree(&ts.objects, "new", 40);
    let (_, keys_dead) = store_dir_tree(&ts.objects, "dead", 40); // never referenced
    let first = store_commit(&ts.objects, tree_old, &[]);
    let tip = store_commit(&ts.objects, tree_new, &[first]);
    let holder = crate::fstree::encode_dir_leaf(&[crate::fstree::Entry {
        name: b"vendor".to_vec(),
        mode: 0o040755,
        content_key: tip.as_bytes().to_vec(),
        ..Default::default()
    }])
    .unwrap();
    ts.objects.put(holder.key, &holder.bytes).unwrap();
    // The completeness walk goes through the commit too.
    put_test_ref(&c, &ts.refs, "site", holder.key);

    backdate_packs(&ts);
    let stats = c.run(0.0).unwrap();
    assert!(!stats.reaped.is_empty(), "nothing reaped: {stats:?}");
    for k in keys_old
        .iter()
        .chain(&keys_new)
        .chain(&[first, tip, holder.key])
    {
        if let Err(e) = ts.objects.get(*k) {
            panic!("key {k}, reachable through the commit the directory holds: {e}");
        }
    }
    assert!(
        count_gone(&ts.objects, &keys_dead) > 0,
        "no unreferenced key was collected, so the test proves nothing"
    );

    rm_test_ref(&c, &ts.refs, "site", holder.key);
    backdate_packs(&ts);
    c.run(0.0).unwrap();
    assert!(
        count_gone(&ts.objects, &keys_old) > 0 && count_gone(&ts.objects, &keys_new) > 0,
        "the held commit's history was not collected after the directory's reference went"
    );
}

#[test]
fn prepare_ref_missing_ancestor_fails() {
    let ts = new_test_store(1 << 20);
    let c = open_collector(&ts, Options::default());
    let (tree, _) = store_dir_tree(&ts.objects, "tree", 4);
    let (absent, _) = new_commit(tree, &[]); // built, never stored
    let tip = store_commit(&ts.objects, tree, &[absent]);
    let err = c
        .prepare_ref(tip)
        .expect_err("prepare_ref accepted a commit whose parent is missing");
    assert!(
        err.to_string().starts_with("gc: walking root "),
        "err = {err}"
    );
}

// ---------------------------------------------------------------------------
// Rust-only: advisory reachability.

#[test]
fn unreachable_from_separates_reached_from_unreached() {
    let ts = new_test_store(1 << 20);
    let c = open_collector(&ts, Options::default());
    let (root_a, keys_a) = store_tree(&ts.objects, "a", 4);
    let (root_b, keys_b) = store_tree(&ts.objects, "b", 4);

    let candidates: Vec<Key> = keys_a.iter().chain(keys_b.iter()).copied().collect();
    let dead = c.unreachable_from(&[root_a], &candidates).unwrap();
    for k in &keys_a {
        assert!(!dead.contains(k), "a-key reported unreachable from root a");
    }
    for k in &keys_b {
        assert!(dead.contains(k), "b-key reported reachable from root a");
    }

    // Both roots together reach everything.
    let dead = c.unreachable_from(&[root_a, root_b], &candidates).unwrap();
    assert!(dead.is_empty(), "dead = {dead:?}, want none");

    // An empty root set reaches nothing, so every candidate comes back, in order.
    let dead = c.unreachable_from(&[], &candidates).unwrap();
    assert_eq!(dead, candidates, "with no roots");
}

#[test]
fn unreachable_from_takes_no_references_into_account() {
    let ts = new_test_store(1 << 20);
    let c = open_collector(&ts, Options::default());
    let (root, keys) = store_tree(&ts.objects, "a", 4);
    put_test_ref(&c, &ts.refs, "held", root);

    // A reference protects the tree from a real cycle, but it is not a root
    // the caller passed, so it does not enter this answer.
    let dead = c.unreachable_from(&[], &keys).unwrap();
    assert_eq!(dead, keys, "a reference must not act as a root here");
}
