//! Ported Go tests of several stores on one directory (`multi_test.go`,
//! `concurrent_view_test.go`, `durable_dedup_test.go`, `process_test.go`).
//!
//! Two stores opened on one directory stand in for two processes: every lock
//! here is a flock, which belongs to the open file, not to the process.

use std::collections::HashSet;
use std::fs::{self, File};
use std::os::fd::AsRawFd;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, Once, mpsc};
use std::thread;
use std::time::Duration;

use tempfile::TempDir;

use crate::amberpack::{REC_HEADER_SIZE, encode_record};
use crate::key::Key;

use super::gc_tests::compact_store;
use super::store_tests::{active_files, obj_seq, sealed_files};
use super::testutil::*;
use super::view::segment_name;
use super::{ACTIVE_SUFFIX, MAGIC_HEADER, Object, Options, Store, unpoison};

fn nosync() -> Options {
    Options::new().sync(false)
}

fn open(dir: &Path) -> Store {
    Store::open(dir).unwrap()
}

fn open_nosync(dir: &Path) -> Store {
    Store::open_with(dir, nosync()).unwrap()
}

/// The id of the active segment `s` owns.
fn own_id(s: &Store) -> u64 {
    unpoison(s.shared.read())
        .active
        .as_ref()
        .expect("the store owns a segment")
        .id
}

/// Whether the store's own active segment holds `k` (Go: `ownCopy`).
fn own_copy(s: &Store, k: Key) -> bool {
    unpoison(s.shared.read())
        .active
        .as_ref()
        .is_some_and(|a| unpoison(a.index.read()).contains_key(&k))
}

/// Lets a test reach the fast path at once: a fresh test directory was
/// modified a moment ago, which the default window, meant for filesystems
/// with coarse timestamps, does not trust yet (Go: `zeroRacyWindow`).
fn zero_racy_window(s: &Store) {
    s.racy_window_nanos.store(0, Ordering::Relaxed);
}

/// Four-KiB incompressible objects, two to an 8 KiB segment.
fn big_objects(n: usize) -> Vec<Object> {
    (0..n)
        .map(|i| {
            let mut data = incompressible(4 << 10);
            data[0] = i as u8;
            data[1] = (i >> 8) as u8;
            blob_obj(&data)
        })
        .collect()
}

#[test]
fn two_stores_open_one_directory() {
    let dir = TempDir::new().unwrap();
    let (a, b) = (open_nosync(dir.path()), open_nosync(dir.path()));
    let objs = test_objects(4);
    put_all(&a, &objs[..2]);
    want_objects(&b, &objs[..2]); // b opened before the writes: a miss, a refresh, a hit
    put_all(&b, &objs[2..]);
    want_objects(&a, &objs[2..]);
}

#[test]
fn many_active_segments_open() {
    let dir = TempDir::new().unwrap();
    let objs = test_objects(6);
    for (i, id) in [1u64, 2, 7].into_iter().enumerate() {
        let (body, _) = build_body(&objs[2 * i..2 * i + 2]);
        fs::write(dir.path().join(segment_name(id, ACTIVE_SUFFIX)), body).unwrap();
    }
    want_objects(&open(dir.path()), &objs);
}

fn try_exclusive(f: &File) -> bool {
    // SAFETY: plain flock(2) on a valid open fd; no memory is involved.
    unsafe { libc::flock(f.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) == 0 }
}

// A release from before this change takes the directory lock exclusively and
// assumes it owns the one active segment. It must not get in while a store is
// open, and a store must not open while it is in.
#[test]
fn old_exclusive_directory_lock_is_refused() {
    let dir = TempDir::new().unwrap();
    let s = open(dir.path());
    let old = File::open(dir.path()).unwrap();
    assert!(
        !try_exclusive(&old),
        "the old exclusive directory lock succeeded while a store is open"
    );
    s.close().unwrap();
    assert!(try_exclusive(&old));
    let err = Store::open(dir.path()).unwrap_err();
    assert!(err.to_string().contains("older release"), "{err}");
}

#[test]
fn second_writer_creates_its_own_segment() {
    let dir = TempDir::new().unwrap();
    let (a, b) = (open_nosync(dir.path()), open_nosync(dir.path()));
    let objs = test_objects(2);
    put_all(&a, &objs[..1]);
    put_all(&b, &objs[1..]);
    assert_eq!(active_files(dir.path()).len(), 2, "want one per writer");
    assert_ne!(own_id(&a), own_id(&b));
}

#[test]
fn writer_adopts_the_largest_unlocked_segment() {
    let dir = TempDir::new().unwrap();
    let (a, b) = (open_nosync(dir.path()), open_nosync(dir.path()));
    let objs = test_objects(7);
    put_all(&a, &objs[..1]);
    put_all(&b, &objs[1..6]);
    let larger = own_id(&b);
    a.close().unwrap();
    b.close().unwrap();
    let c = open_nosync(dir.path());
    put_all(&c, &objs[6..]);
    assert_eq!(own_id(&c), larger, "want the larger idle segment");
    assert_eq!(
        active_files(dir.path()).len(),
        2,
        "want the two that existed"
    );
    want_objects(&c, &objs);
}

#[test]
fn serial_writers_fill_one_segment() {
    let dir = TempDir::new().unwrap();
    let objs = test_objects(6);
    for round in 0..3 {
        let s = open_nosync(dir.path());
        put_all(&s, &objs[2 * round..2 * round + 2]);
        s.close().unwrap();
    }
    assert_eq!(
        (
            active_files(dir.path()).len(),
            sealed_files(dir.path()).len()
        ),
        (1, 0),
        "want one active segment that every writer reused"
    );
    want_objects(&open(dir.path()), &objs);
}

#[test]
fn concurrent_creators_get_distinct_ids() {
    const N: usize = 8;
    let dir = TempDir::new().unwrap();
    let objs = test_objects(N);
    let stores: Vec<Store> = (0..N).map(|_| open_nosync(dir.path())).collect();
    thread::scope(|sc| {
        for (s, o) in stores.iter().zip(&objs) {
            sc.spawn(move || s.put(o.key, &o.data).unwrap());
        }
    });
    let ids: HashSet<u64> = stores.iter().map(own_id).collect();
    assert_eq!((ids.len(), active_files(dir.path()).len()), (N, N));
    assert_eq!(
        files_with_suffix(dir.path(), ".tmp"),
        Vec::<std::path::PathBuf>::new(),
        "temporary files left behind"
    );
    want_objects(&open(dir.path()), &objs);
}

#[test]
fn adopter_completes_a_crashed_seal() {
    let dir = TempDir::new().unwrap();
    let objs = test_objects(5);
    let w = open_nosync(dir.path());
    put_all(&w, &objs[..4]);
    w.close().unwrap();
    crash_seal(dir.path());

    let reader = open(dir.path()); // before anybody has finished the seal
    want_objects(&reader, &objs[..4]);
    assert_eq!(
        sealed_files(dir.path()).len(),
        0,
        "a reader finished the seal"
    );

    let w2 = open_nosync(dir.path());
    put_all(&w2, &objs[4..]);
    assert_eq!(
        (
            sealed_files(dir.path()).len(),
            active_files(dir.path()).len()
        ),
        (1, 1),
        "want the crashed segment sealed and one new active"
    );
    want_objects(&reader, &objs);
    want_objects(&w2, &objs);
}

#[test]
fn reader_sees_what_another_wrote_after_it_opened() {
    let dir = TempDir::new().unwrap();
    let reader = open(dir.path());
    let objs = test_objects(3);

    // Every record seals its segment.
    let sealer = Store::open_with(dir.path(), nosync().segment_size(1)).unwrap();
    put_all(&sealer, &objs[..2]);
    want_objects(&reader, &objs[..2]); // in segments sealed after the reader opened

    let w = open_nosync(dir.path());
    put_all(&w, &objs[2..]);
    want_objects(&reader, &objs[2..]); // in another writer's active segment
}

#[test]
fn reader_holds_no_lock() {
    let dir = TempDir::new().unwrap();
    let objs = test_objects(2);
    let w = open_nosync(dir.path());
    put_all(&w, &objs[..1]);
    w.close().unwrap();
    let reader = open(dir.path()); // say, a pager left open
    want_objects(&reader, &objs[..1]);

    let w2 = open_nosync(dir.path());
    put_all(&w2, &objs[1..]);
    assert_eq!(
        active_files(dir.path()).len(),
        1,
        "the reader kept the writer from adopting the idle segment"
    );
    want_objects(&reader, &objs);
}

#[test]
fn long_lived_reader_survives_seal_and_compaction() {
    let (dir, s, objs) = compact_store(); // objs[0..2] and objs[2..4] sealed, objs[4] active
    let reader = open(dir.path());
    want_objects(&reader, &objs);

    s.compact(live_at(&objs, &[0, 2, 4]), sweep_opts()).unwrap();
    // The two half-dead segments are gone from disk. The reader's mappings of
    // them stay valid, and the survivors are readable either way.
    want_objects(
        &reader,
        &[objs[0].clone(), objs[2].clone(), objs[4].clone()],
    );

    let mut data = incompressible(4 << 10);
    data[0] = 0xee;
    let fresh = blob_obj(&data);
    put_all(&s, std::slice::from_ref(&fresh));
    want_objects(&reader, std::slice::from_ref(&fresh)); // a miss: the reader looks again

    for g in &unpoison(reader.shared.read()).sealed {
        assert!(
            g.path.exists(),
            "the reader still lists segment {:x}, which is gone",
            g.id
        );
    }
}

#[test]
fn dedup_does_not_refresh() {
    let dir = TempDir::new().unwrap();
    let s = open_nosync(dir.path());
    let objs = test_objects(61);
    // A store's first write span looks at the directory once: it may have
    // opened during a sweep (gate.rs).
    put_all(&s, &objs[60..]);
    let before = s.refreshes.load(Ordering::Relaxed);
    s.write_batch(obj_seq(&objs[..50], None)).unwrap();
    put_all(&s, &objs[50..60]);
    assert_eq!(
        s.refreshes.load(Ordering::Relaxed),
        before,
        "writing new objects listed the directory; the duplicate check must not"
    );
}

#[test]
fn missing_refreshes_once() {
    let dir = TempDir::new().unwrap();
    let s = open_nosync(dir.path());
    let keys: Vec<Key> = test_objects(100).iter().map(|o| o.key).collect();
    let before = s.refreshes.load(Ordering::Relaxed);
    assert_eq!(s.missing(&keys).unwrap().len(), keys.len());
    assert_eq!(
        s.refreshes.load(Ordering::Relaxed) - before,
        1,
        "missing lists the directory once"
    );
}

// A view of somebody else's active segment is checked on every read: if the
// record is not the one the entry promised, the read fails instead of
// returning another object's bytes.
#[test]
fn foreign_read_never_returns_the_wrong_record() {
    let dir = TempDir::new().unwrap();
    let w = open(dir.path());
    let objs = test_objects(1);
    put_all(&w, &objs);
    let reader = open(dir.path());
    want_objects(&reader, &objs);

    // A byte of the record's key.
    flip_byte(&only_active(dir.path()), MAGIC_HEADER.len() as u64 + 1 + 5);
    let err = reader.get(objs[0].key).unwrap_err();
    assert!(err.is_corrupt(), "get = {err}; want a corrupt error");
}

#[test]
fn wipe_refuses_while_another_store_owns_a_segment() {
    let dir = TempDir::new().unwrap();
    let (a, b) = (open_nosync(dir.path()), open_nosync(dir.path()));
    let objs = test_objects(2);
    put_all(&a, &objs[..1]);
    put_all(&b, &objs[1..]);
    assert!(
        a.wipe().is_err(),
        "wipe deleted a segment that another store is writing to"
    );
    want_objects(&a, &objs);
    b.close().unwrap();
    a.wipe().unwrap();
    assert_eq!(
        (
            active_files(dir.path()).len(),
            sealed_files(dir.path()).len()
        ),
        (0, 0)
    );
    assert!(!a.has(objs[1].key).unwrap());
}

#[test]
fn stale_temporary_segment_is_removed() {
    let dir = TempDir::new().unwrap();
    let stale = dir
        .path()
        .join(format!("{}.tmp", segment_name(0xff, ACTIVE_SUFFIX)));
    fs::write(&stale, MAGIC_HEADER).unwrap();
    let s = open_nosync(dir.path());
    put_all(&s, &test_objects(1));
    assert!(
        !stale.exists(),
        "the temporary file of a crashed creation is still there"
    );
}

// A lookup that finds nothing must not list the directory when nothing in it
// can have changed: listing costs a stat per segment, a thousand times a miss.
#[test]
fn misses_do_not_list_an_unchanged_directory() {
    let dir = TempDir::new().unwrap();
    let w = Store::open_with(dir.path(), nosync().segment_size(1)).unwrap();
    let objs = test_objects(40);
    put_all(&w, &objs[..3]);
    w.close().unwrap();
    let s = open(dir.path());
    zero_racy_window(&s);
    let before = s.refreshes.load(Ordering::Relaxed);
    for o in &objs[3..] {
        assert!(!s.has(o.key).unwrap());
        assert!(s.get(o.key).unwrap_err().is_not_found());
    }
    assert_eq!(
        s.refreshes.load(Ordering::Relaxed),
        before,
        "lookups that found nothing listed an unchanged directory"
    );
}

#[test]
fn fast_path_still_sees_other_stores_writes() {
    let dir = TempDir::new().unwrap();
    let reader = open(dir.path());
    zero_racy_window(&reader);
    let objs = test_objects(5);

    let w = open_nosync(dir.path());
    put_all(&w, &objs[..1]);
    want_objects(&reader, &objs[..1]); // a new active segment: the directory changed
    put_all(&w, &objs[1..2]);
    want_objects(&reader, &objs[1..2]); // the same segment grew: only its size changed

    let sealer = Store::open_with(dir.path(), nosync().segment_size(1)).unwrap();
    put_all(&sealer, &objs[2..3]);
    want_objects(&reader, &objs[2..3]); // a segment created and sealed in one go

    let before = reader.refreshes.load(Ordering::Relaxed);
    assert!(!reader.has(objs[4].key).unwrap());
    assert_eq!(
        reader.refreshes.load(Ordering::Relaxed),
        before,
        "a miss listed a directory that had not changed"
    );
    put_all(&w, &objs[3..4]);
    want_objects(&reader, &objs[3..4]); // and the fast path did not stick
}

// ---------------------------------------------------------------------------
// The view under concurrent change (Go: concurrent_view_test.go).

// A lookup that finds nothing lists the directory. While compact or remove
// deletes segments — out of the view already, not yet out of the directory —
// such a listing must not map one again: the duplicate check would go on
// finding objects that are gone, and a write of one would be skipped.
#[test]
fn a_lookup_between_detach_and_unlink_does_not_bring_victims_back() {
    for which in ["compact", "remove"] {
        let (_dir, s, objs) = compact_store(); // objs[0..2] in the first sealed segment
        let s = Arc::new(s);
        let absent = blob_obj(b"nobody stored this").key;
        let (tx, looked) = mpsc::channel::<()>();
        let tx = Mutex::new(Some(tx));
        let hooked = s.clone();
        *unpoison(s.hooks.after_detach.lock()) = Some(Box::new(move || {
            if let Some(tx) = unpoison(tx.lock()).take() {
                let s = hooked.clone();
                thread::spawn(move || {
                    let _ = s.has(absent);
                    let _ = tx.send(());
                });
            }
            // Ample for the lookup to list the directory, were it let.
            thread::sleep(Duration::from_millis(150));
        }));
        if which == "compact" {
            s.compact(live_at(&objs, &[0, 2, 3, 4]), sweep_opts())
                .unwrap();
        } else {
            let id = unpoison(s.shared.read()).sealed[0].id;
            s.remove(id).unwrap();
        }
        looked.recv_timeout(Duration::from_secs(10)).unwrap();
        assert!(
            !s.has_local(objs[1].key).unwrap(),
            "{which}: a listing between the segment leaving the view and leaving the directory mapped it again"
        );
        *unpoison(s.hooks.after_detach.lock()) = None; // the hook holds the store
    }
}

// The same under load: lookups that find nothing, all through a pass that
// removes every segment.
#[test]
fn lookups_during_compact() {
    let dir = TempDir::new().unwrap();
    let s = Store::open_with(dir.path(), nosync().segment_size(8 << 10)).unwrap();
    let objs = big_objects(300);
    put_all(&s, &objs); // two to a segment
    // Every segment half dead: all of them victims.
    let live: Vec<usize> = (0..objs.len()).step_by(2).collect();
    let absent = blob_obj(b"nobody stored this").key;
    let stop = AtomicBool::new(false);
    thread::scope(|sc| {
        for _ in 0..4 {
            sc.spawn(|| {
                while !stop.load(Ordering::Relaxed) {
                    assert!(!s.has(absent).unwrap());
                }
            });
        }
        let res = s.compact(live_at(&objs, &live), sweep_opts());
        stop.store(true, Ordering::Relaxed);
        res.unwrap();
    });
    let ghosts = (1..objs.len())
        .step_by(2)
        .filter(|&i| s.has_local(objs[i].key).unwrap())
        .count();
    assert_eq!(ghosts, 0, "the duplicate check still finds reaped objects");
}

// The store's first write takes an idle segment for its own and drops the
// read-only view it had of it. A lookup's refresh may be reading that view at
// that moment; it must not fail the lookup.
#[test]
fn lookups_during_the_first_write_see_no_error() {
    let objs = test_objects(3);
    let absent = blob_obj(b"nobody stored this").key;
    for round in 0..40 {
        let dir = TempDir::new().unwrap();
        let w = open_nosync(dir.path());
        put_all(&w, &objs[..2]);
        w.close().unwrap(); // leaves an idle segment, which the first write below adopts
        let s = open_nosync(dir.path());
        let stop = AtomicBool::new(false);
        thread::scope(|sc| {
            let looking = sc.spawn(|| {
                while !stop.load(Ordering::Relaxed) {
                    s.has(absent)?;
                }
                Ok::<(), super::Error>(())
            });
            s.put(objs[2].key, &objs[2].data).unwrap();
            stop.store(true, Ordering::Relaxed);
            if let Err(e) = looking.join().unwrap() {
                panic!("round {round}: a lookup during the store's first write: {e}");
            }
        });
    }
}

// A refresh that fails half way must leave the view as it was. One that had
// already read another store's new records, and then failed on something
// else, must not keep its place in that segment and drop the records: no
// later refresh would find them again.
#[test]
fn a_failed_refresh_loses_nothing() {
    // SAFETY: geteuid(2) has no preconditions.
    if unsafe { libc::geteuid() } == 0 {
        return; // root reads a file that grants nobody anything
    }
    let objs = test_objects(2);
    // The refresh visits segments in map order: either one may come first.
    for round in 0..24 {
        let dir = TempDir::new().unwrap();
        let w = open_nosync(dir.path());
        let reader = open(dir.path());
        put_all(&w, &objs[..1]);
        want_objects(&reader, &objs[..1]); // the reader follows w's segment from here on
        put_all(&w, &objs[1..]);

        let bad = dir.path().join(segment_name(0xff, ACTIVE_SUFFIX));
        fs::write(&bad, MAGIC_HEADER).unwrap();
        fs::set_permissions(&bad, fs::Permissions::from_mode(0o000)).unwrap();
        let _ = reader.get(objs[1].key); // may fail, fairly: the refresh cannot read that file
        fs::remove_file(&bad).unwrap();
        if let Err(e) = reader.get(objs[1].key) {
            panic!("round {round}: after the unreadable file was removed: {e}");
        }
    }
}

// A segment that was listed and is gone when the refresh comes to open it
// means the listing is out of date: what the segment held may have moved to
// one created since. The refresh lists again rather than settle for less.
#[test]
fn refresh_lists_again_when_a_listed_segment_vanished() {
    let dir = TempDir::new().unwrap();
    let reader = open(dir.path()); // an empty view
    let c = Arc::new(Store::open_with(dir.path(), nosync().segment_size(8 << 10)).unwrap());
    let objs = big_objects(5);
    put_all(&c, &objs); // objs[0..2] and objs[2..4] sealed, objs[4] active

    let failed: Arc<Mutex<Option<String>>> = Arc::default();
    let (once, compactor, slot, live) = (
        Once::new(),
        c.clone(),
        failed.clone(),
        live_at(&objs, &[0, 2, 3, 4]),
    );
    let live = Arc::new(live);
    *unpoison(reader.hooks.after_list.lock()) = Some(Box::new(move || {
        // A whole pass between the listing and the opening of what it lists.
        once.call_once(|| {
            let live = live.clone();
            if let Err(e) = compactor.compact(move |k| live(k), sweep_opts()) {
                *unpoison(slot.lock()) = Some(e.to_string());
            }
        });
    }));
    // Moved by the pass, into a segment the listing does not have.
    let got = reader.get(objs[0].key);
    assert_eq!(*unpoison(failed.lock()), None, "the pass failed");
    assert_eq!(got.unwrap(), objs[0].data);
}

// ---------------------------------------------------------------------------
// Durability across stores (Go: durable_dedup_test.go).

// A store that syncs acknowledges a write only when losing power would not
// lose the object. A copy in another store's active segment counts as a
// duplicate only as far as that store has synced it.
#[test]
fn dedup_does_not_rely_on_another_stores_unsynced_records() {
    let objs = test_objects(1);
    let k = objs[0].key;

    let dir = TempDir::new().unwrap();
    let unsynced = open_nosync(dir.path()); // written, never synced, its writer still open
    put_all(&unsynced, &objs);
    let s = open(dir.path()); // syncs
    want_objects(&s, &objs);
    put_all(&s, &objs);
    assert!(
        own_copy(&s, k),
        "a syncing store acknowledged a write whose only copy another store has yet to sync"
    );

    let dir = TempDir::new().unwrap();
    let synced = open(dir.path()); // written and synced
    put_all(&synced, &objs);
    let s = open(dir.path());
    want_objects(&s, &objs);
    put_all(&s, &objs);
    assert!(
        !own_copy(&s, k),
        "a second copy was written of a record another store had synced"
    );
}

// Compaction must not delete a synced copy on the strength of one that a live
// writer has yet to sync.
#[test]
fn compact_does_not_leave_the_only_copy_unsynced() {
    let mut objs = big_objects(2);
    let (kept, garbage) = (objs.remove(0), objs.remove(0));

    let dir = TempDir::new().unwrap();
    let unsynced = open_nosync(dir.path()); // a live writer that never syncs
    put_all(&unsynced, std::slice::from_ref(&kept));

    let s = Store::open_with(dir.path(), Options::new().segment_size(8 << 10)).unwrap(); // syncs
    let rec = encode_record(kept.key, &kept.data).unwrap();
    assert!(rec.len() > REC_HEADER_SIZE);
    s.append_record(kept.key, &rec).unwrap(); // a copy of its own, whatever the duplicate check thinks
    put_all(&s, std::slice::from_ref(&garbage)); // fills the segment: sealed, and synced
    let only_kept = kept.key;
    s.compact(move |k| k == only_kept, sweep_opts()).unwrap();

    let sealed_copy = unpoison(s.shared.read())
        .sealed
        .iter()
        .any(|g| g.has(kept.key));
    assert!(
        sealed_copy || own_copy(&s, kept.key),
        "the pass deleted a synced copy and left only the one another store has yet to sync"
    );
}

// ---------------------------------------------------------------------------
// A real second process (Go: process_test.go).

const CHILD_DIR_ENV: &str = "PACKSTORE_TEST_CHILD_DIR";
const CHILD_DATA: &[u8] = b"written by the child process";

/// The second process of the test below: with `CHILD_DIR_ENV` set, it opens
/// that store, writes one object and exits. Run as an ordinary test it does
/// nothing (Go: the `TestMain` child).
#[test]
fn child_process_entry() {
    let Ok(dir) = std::env::var(CHILD_DIR_ENV) else {
        return;
    };
    let s = Store::open_with(&dir, nosync()).unwrap();
    let o = blob_obj(CHILD_DATA);
    s.put(o.key, &o.data).unwrap();
    s.close().unwrap();
}

#[test]
fn second_process_writes_are_visible() {
    let dir = TempDir::new().unwrap();
    // Open, and owning a segment, for the child's whole life.
    let s = open_nosync(dir.path());
    put_all(&s, &test_objects(1));

    let child = Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "packstore::multi_tests::child_process_entry",
            "--test-threads=1",
        ])
        .env(CHILD_DIR_ENV, dir.path())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let _bound = watchdog(child.id(), Duration::from_secs(60));
    let out = child.wait_with_output().unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        out.status.success() && stdout.contains("1 passed"),
        "child process: {:?}\n{stdout}\n{}",
        out.status,
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(s.get(blob_obj(CHILD_DATA).key).unwrap(), CHILD_DATA);
    assert_eq!(
        active_files(dir.path()).len(),
        2,
        "want the parent's and the child's"
    );
}

/// `write_parallel` is what `ingest::dir` writes through: its duplicate check
/// must not count a record whose only copy another store has yet to sync.
#[test]
fn write_parallel_does_not_rely_on_another_stores_unsynced_records() {
    let objs = test_objects(1);
    let k = objs[0].key;
    let dir = TempDir::new().unwrap();
    let unsynced = open_nosync(dir.path()); // written, never synced, its writer still open
    put_all(&unsynced, &objs);
    let s = open(dir.path()); // syncs
    want_objects(&s, &objs); // s has looked: the record is in its view of the other's segment
    let (_stats, res) = s.write_parallel(obj_seq(&objs, None), super::WriteOpts::default());
    res.unwrap();
    assert!(
        own_copy(&s, k),
        "write_parallel acknowledged a write whose only copy another store has yet to sync"
    );
}

/// Ids wrap as Go's uint64 arithmetic does. A name with the highest id — a
/// stale temporary counts, the listing folds it into the maximum — made a
/// debug build's first write panic with "attempt to add with overflow".
#[test]
fn highest_segment_id_does_not_stop_the_first_write() {
    let dir = TempDir::new().unwrap();
    fs::write(
        dir.path()
            .join(format!("{}.tmp", segment_name(u64::MAX, ACTIVE_SUFFIX))),
        MAGIC_HEADER,
    )
    .unwrap();
    let s = open_nosync(dir.path());
    put_all(&s, &test_objects(1));
    want_objects(&s, &test_objects(1));
}
