//! Ported Go tests of the collector across stores and processes
//! (`multi_test.go`, `process_test.go`, `span_test.go`).
//!
//! Two store/refstore/collector triples on one directory stand in for two
//! processes: every lock between them is a file lock or the reference
//! database's own.

use std::io::{BufRead, BufReader, Read};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::fstree::{self, encode_blob};
use crate::key::Key;
use crate::packstore;
use crate::reference::Reference;
use crate::refstore;

use super::tests::{
    HOUR, TestStore, backdate_packs, count_gone, new_test_store, open_collector, put_test_ref,
    rm_test_ref, store_tree,
};
use super::{Collector, Options, Span, lock};

/// A wait that must end.
const MULTI_WAIT: Duration = Duration::from_secs(20);
/// How long "still waiting" is watched.
const MULTI_QUIET: Duration = Duration::from_millis(150);

/// Runs `f` on a thread; the receiver yields what it returned (Go: `async`).
fn run<T: Send + 'static>(f: impl FnOnce() -> T + Send + 'static) -> mpsc::Receiver<T> {
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        let _ = tx.send(f());
    });
    rx
}

fn still_waiting<T>(rx: &mpsc::Receiver<T>, what: &str) {
    assert!(
        matches!(
            rx.recv_timeout(MULTI_QUIET),
            Err(mpsc::RecvTimeoutError::Timeout)
        ),
        "{what} did not wait"
    );
}

fn finishes<T>(rx: &mpsc::Receiver<T>, what: &str) -> T {
    rx.recv_timeout(MULTI_WAIT)
        .unwrap_or_else(|_| panic!("{what} is still waiting"))
}

fn text<T, E: std::fmt::Display>(r: Result<T, E>) -> Result<(), String> {
    r.map(drop).map_err(|e| e.to_string())
}

/// Another store pair on an existing test directory (Go: `openTestStoreAt`).
struct Peer {
    dir: PathBuf,
    objects: Arc<packstore::Store>,
    refs: Arc<refstore::Store>,
}

fn open_peer(dir: &Path, seg_size: u64) -> Peer {
    let objects = packstore::Store::open_with(
        dir.join("packstore"),
        packstore::Options::new().segment_size(seg_size),
    )
    .unwrap();
    Peer {
        dir: dir.to_path_buf(),
        objects: Arc::new(objects),
        refs: Arc::new(refstore::Store::open(dir.join("refs"), true).unwrap()),
    }
}

fn peer_collector(p: &Peer, opts: Options) -> Collector {
    Collector::open(
        p.dir.join("closures"),
        Arc::clone(&p.objects),
        Arc::clone(&p.refs),
        opts,
    )
    .unwrap()
}

fn grace(grace: Duration) -> Options {
    Options {
        grace,
        ..Options::default()
    }
}

/// Go: `activeSegments`.
fn active_segments(dir: &Path) -> Vec<PathBuf> {
    let mut found: Vec<_> = std::fs::read_dir(dir.join("packstore"))
        .unwrap()
        .map(|e| e.unwrap().path())
        .filter(|p| p.to_string_lossy().ends_with(".seg.active"))
        .collect();
    found.sort();
    found
}

fn encode_ref(name: &str, root: Key) -> Vec<u8> {
    Reference {
        name: name.to_string(),
        key: root.as_bytes().to_vec(),
        user: String::new(),
        created_at: SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos() as i64,
        signature: Vec::new(),
        public_key: Vec::new(),
    }
    .encode()
    .unwrap()
}

/// A reference put inside a span the caller holds.
fn put_ref_in_span(span: &Span<'_>, refs: &refstore::Store, name: &str, root: Key) {
    let prepared = span.prepare_ref(root).unwrap();
    refs.put(name, &encode_ref(name, root)).unwrap();
    prepared.commit();
}

#[test]
fn cycle_waits_for_a_foreign_write_span() {
    let a = new_test_store(4 << 10);
    let b = open_peer(&a.dir, 4 << 10);
    let cb = peer_collector(&b, grace(HOUR));

    let span = a.objects.begin_write().unwrap();
    let ran = run(move || text(cb.run(0.0)));
    still_waiting(&ran, "a cycle, while another store is in a write span,");
    drop(span);
    finishes(&ran, "the cycle, after the write span ended,").unwrap();
}

// A reference put in another store must not land between a cycle's snapshot
// of the roots and its sweep: the cycle would not know the new root, and the
// barrier that protects such a put lives in the cycle's own process.
#[test]
fn foreign_prepare_ref_waits_for_a_cycle() {
    let a = new_test_store(4 << 10);
    let b = open_peer(&a.dir, 4 << 10);
    let ca = open_collector(&a, grace(HOUR));
    let cb = peer_collector(&b, grace(HOUR));
    let (root, _) = store_tree(&a.objects, "tree", 8);

    // Dropped, also when an assertion fails, which lets the cycle go: the
    // collector's close waits for it.
    let (hold, held) = mpsc::channel::<()>();
    let (reached_tx, reached) = mpsc::channel::<()>();
    let (held, reached_tx) = (Mutex::new(held), Mutex::new(reached_tx));
    lock(&cb.core.mu).mid_mark = Some(Arc::new(move || {
        let _ = lock(&reached_tx).send(());
        let _ = lock(&held).recv();
    }));
    let ran = run(move || text(cb.run(0.0)));
    finishes(&reached, "the cycle, on its way to the mark,");

    let prepared = run(move || text(ca.prepare_ref(root).map(|p| p.commit())));
    still_waiting(
        &prepared,
        "a reference put in another store, during a cycle,",
    );
    drop(hold);
    finishes(&ran, "the cycle").unwrap();
    finishes(&prepared, "the reference put, after the cycle ended,").unwrap();
}

// One store ingests, publishes and deletes while the other collects as fast
// as it can, with no grace period to hide behind. The writer brackets each
// ingest and its reference put in one span, so that a cycle never finds
// objects whose reference is still to come. (Go's test opens the span on the
// store and puts the reference through the collector; its writing collector
// runs no cycle, so the order of the two locks does not matter there. This
// port uses the collector's span, the pattern the documents recommend.)
#[test]
fn collect_while_another_store_ingests() {
    let a = new_test_store(4 << 10);
    let b = open_peer(&a.dir, 4 << 10);
    let ca = open_collector(&a, grace(Duration::from_nanos(1)));
    let cb = peer_collector(&b, grace(Duration::from_nanos(1)));

    let stop = AtomicBool::new(false);
    let (mut kept, mut dead) = (Vec::new(), Vec::new());
    let (cycles, reaped) = thread::scope(|sc| {
        let collecting = sc.spawn(|| {
            let (mut cycles, mut reaped) = (0usize, 0usize);
            while !stop.load(Ordering::Relaxed) {
                let stats = cb.run(0.0).map_err(|e| e.to_string())?;
                cycles += 1;
                reaped += stats.reaped.len();
            }
            Ok::<_, String>((cycles, reaped))
        });
        for i in 0..16 {
            let name = format!("ref-{i}");
            let span = ca.begin_span().unwrap();
            let (root, keys) = store_tree(&a.objects, &format!("tree-{i}"), 30);
            put_ref_in_span(&span, &a.refs, &name, root);
            drop(span);
            if i % 2 == 1 {
                rm_test_ref(&ca, &a.refs, &name, root);
                dead.push(keys);
            } else {
                kept.push(root);
            }
        }
        stop.store(true, Ordering::Relaxed);
        collecting.join().unwrap()
    })
    .unwrap_or_else(|e| panic!("a cycle failed: {e}"));
    cb.run(0.0).unwrap(); // a quiet one, for the last deletions
    println!("{cycles} cycles ran alongside the ingest, reaping {reaped} segments");

    for objects in [&a.objects, &b.objects] {
        for &root in &kept {
            if let Err(e) = fstree::check_complete(root, |k| objects.get(k), |k| objects.has(k), 4)
            {
                panic!("a kept reference is no longer complete: {e}");
            }
        }
    }
    let gone: usize = dead.iter().map(|keys| count_gone(&b.objects, keys)).sum();
    assert!(
        gone > 0,
        "nothing was collected, so the test proves nothing"
    );
}

// A small store's segments never fill, and nobody may be around to seal them:
// a cycle seals the active segments no writer holds, so that they can be
// collected.
#[test]
fn idle_foreign_active_segment_is_collected() {
    let a = new_test_store(1 << 20);
    let ca = open_collector(&a, grace(HOUR));
    let (root, keys) = store_tree(&a.objects, "dead", 40);
    put_test_ref(&ca, &a.refs, "dead", root);
    rm_test_ref(&ca, &a.refs, "dead", root);
    let (root_keep, keys_keep) = store_tree(&a.objects, "keep", 4);
    put_test_ref(&ca, &a.refs, "keep", root_keep);
    a.objects.close().unwrap(); // the writer is gone; its segment stays, unsealed
    assert_eq!(
        active_segments(&a.dir).len(),
        1,
        "want the writer's one active segment"
    );

    let b = open_peer(&a.dir, 1 << 20);
    let cb = peer_collector(&b, grace(HOUR));
    cb.run(0.0).unwrap();
    assert_eq!(
        active_segments(&a.dir),
        Vec::<PathBuf>::new(),
        "the idle segment was not sealed"
    );
    backdate_packs(&a);
    cb.run(0.0).unwrap();
    assert!(
        count_gone(&b.objects, &keys) > 0,
        "nothing of the idle segment's garbage was collected"
    );
    assert_eq!(
        count_gone(&b.objects, &keys_keep),
        0,
        "live objects were collected with it"
    );
}

#[test]
fn owned_foreign_active_segment_is_left_alone() {
    let a = new_test_store(1 << 20);
    let ca = open_collector(&a, grace(HOUR));
    let (root, keys) = store_tree(&a.objects, "dead", 40);
    put_test_ref(&ca, &a.refs, "dead", root);
    rm_test_ref(&ca, &a.refs, "dead", root);
    let owned = active_segments(&a.dir);
    assert_eq!(owned.len(), 1);

    let b = open_peer(&a.dir, 1 << 20);
    let cb = peer_collector(&b, grace(HOUR));
    for _ in 0..2 {
        cb.run(0.0).unwrap();
        backdate_packs(&a);
    }
    assert_eq!(
        active_segments(&a.dir),
        owned,
        "want the live writer's segment untouched"
    );
    assert_eq!(
        count_gone(&a.objects, &keys),
        0,
        "objects vanished from a segment its writer still holds"
    );
    let (_, keys_more) = store_tree(&a.objects, "more", 4);
    assert_eq!(
        count_gone(&b.objects, &keys_more),
        0,
        "the writer's later objects are not visible to the other store"
    );
}

// ---------------------------------------------------------------------------
// Spans (Go: span_test.go).

// Status reads the references, which every process sees at once, and marks
// from them. The objects they name may have been written by another process
// since this one last looked at the store: status has to look first.
#[test]
fn status_sees_what_another_store_wrote_and_named() {
    let a = new_test_store(1 << 20);
    let ca = open_collector(&a, Options::default());
    let b = open_peer(&a.dir, 1 << 20);
    let cb = peer_collector(&b, Options::default());
    cb.status().unwrap(); // b has looked at the store
    let (root, keys) = store_tree(&a.objects, "named elsewhere", 5);
    put_test_ref(&ca, &a.refs, "main", root);
    let st = cb
        .status()
        .unwrap_or_else(|e| panic!("status, after another store wrote a tree and named it: {e}"));
    assert_eq!((st.refs, st.marked), (1, keys.len()));
}

// A write inside a collector's write span must not wait for a wipe that is
// waiting for the span.
#[test]
fn writes_inside_a_collector_span_pass_a_waiting_wipe() {
    let ts = new_test_store(1 << 20);
    let c = Arc::new(open_collector(&ts, Options::default()));
    let gate = c.begin_write(); // dropped, letting everybody go, when the test fails half way
    let (wiper, objects) = (Arc::clone(&c), Arc::clone(&ts.objects));
    let wiped = run(move || text(wiper.wipe(|| objects.wipe())));
    still_waiting(
        &wiped,
        "a wipe, while a write span of its own process is open,",
    );

    let (objects, o) = (
        Arc::clone(&ts.objects),
        encode_blob(b"written inside the span"),
    );
    let wrote = run(move || text(objects.put(o.key, &o.bytes)));
    finishes(
        &wrote,
        "a write inside the span, with a wipe waiting for the span,",
    )
    .unwrap();
    drop(gate);
    finishes(&wiped, "the wipe, after the span ended,").unwrap();
}

// A span opened through the collector takes the reference lock before the
// store's gate, the order a cycle takes them in, and prepares references
// without taking either again. So a reference can be put inside it while a
// cycle of the same process waits for it to end.
#[test]
fn span_puts_a_reference_while_a_cycle_waits() {
    let ts = new_test_store(1 << 20);
    let c = Arc::new(open_collector(&ts, Options::default()));
    let span = c.begin_span().unwrap();
    let (root, keys) = store_tree(&ts.objects, "inside a span", 5);

    let cycler = Arc::clone(&c);
    let ran = run(move || text(cycler.run(0.0)));
    still_waiting(&ran, "a cycle, while a span of its own process is open,");

    // On this thread: the span cannot leave it. A put that waited for the
    // cycle would hang here, and the harness would say so.
    put_ref_in_span(&span, &ts.refs, "main", root);
    drop(span);
    finishes(&ran, "the cycle, after the span ended,").unwrap();
    assert_eq!(
        count_gone(&ts.objects, &keys),
        0,
        "the reference names these objects"
    );
}

// The order is the cycle's: reference lock, then gate. The other way round
// deadlocks: a cycle holds the reference lock and waits at the gate for a span
// on the store; a collector span joins that span at the gate and then waits
// for the reference lock; the store's span ends, and the cycle still waits
// for the one that waits for it.
#[test]
fn begin_span_takes_the_reference_lock_before_the_gate() {
    let ts = new_test_store(1 << 20);
    let c = Arc::new(open_collector(&ts, Options::default()));
    let outer = ts.objects.begin_write().unwrap(); // a span on the store, not the collector's
    let cycler = Arc::clone(&c);
    let ran = run(move || text(cycler.run(0.0)));
    still_waiting(&ran, "a cycle, while a span on the store is open,");
    let spanner = Arc::clone(&c);
    let spanned = run(move || text(spanner.begin_span().map(|s| s.end())));
    still_waiting(
        &spanned,
        "a collector span, behind a cycle that holds the reference lock,",
    );
    drop(outer);
    finishes(&ran, "the cycle, after the store's span ended,").unwrap();
    finishes(&spanned, "the collector's span, after the cycle,").unwrap();
}

// A mark that fails gives the gate back like any other cycle. Kept, every
// write of every other process would wait until this collector's store closes.
#[test]
fn a_failed_mark_releases_the_gate() {
    let ts = new_test_store(1 << 20);
    let c = open_collector(&ts, Options::default());
    // A reference to an object that is not there: the mark aborts loudly.
    let absent = encode_blob(b"never stored").key;
    ts.refs
        .put("dangling", &encode_ref("dangling", absent))
        .unwrap();
    let err = text(c.run(0.0)).unwrap_err();
    assert!(err.contains("missing from store"), "{err}");

    let peer = open_peer(&ts.dir, 1 << 20);
    let (objects, o) = (
        Arc::clone(&peer.objects),
        encode_blob(b"written after the failed cycle"),
    );
    let wrote = run(move || text(objects.put(o.key, &o.bytes)));
    finishes(
        &wrote,
        "a write of another store, after a cycle whose mark failed,",
    )
    .unwrap();
}

// ---------------------------------------------------------------------------
// A real second process (Go: process_test.go).

const CHILD_DIR_ENV: &str = "GC_TEST_CHILD_DIR";
const IN_SPAN: &str = "in a write span";

/// The second process of the test below: with `CHILD_DIR_ENV` set, it opens
/// that store, enters a write span, says so, and stays in it until its stdin
/// closes. Run as an ordinary test it does nothing (Go: the `TestMain` child).
#[test]
fn child_process_entry() {
    let Ok(dir) = std::env::var(CHILD_DIR_ENV) else {
        return;
    };
    let objects = packstore::Store::open(Path::new(&dir).join("packstore")).unwrap();
    let span = objects.begin_write().unwrap();
    println!("{IN_SPAN}");
    let mut sink = Vec::new();
    let _ = std::io::stdin().read_to_end(&mut sink); // until the parent has seen enough
    drop(span);
    objects.close().unwrap();
}

#[test]
fn cycle_waits_for_another_process() {
    let ts: TestStore = new_test_store(4 << 10);
    let c = open_collector(&ts, grace(HOUR));

    let mut child = Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "gc::multi_tests::child_process_entry",
            "--nocapture",
            "--test-threads=1",
        ])
        .env(CHILD_DIR_ENV, &ts.dir)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    // A child that hangs must fail this test, not hang it.
    let _bound = packstore::testutil::watchdog(child.id(), Duration::from_secs(60));
    let stdin = child.stdin.take().unwrap();
    let mut lines = BufReader::new(child.stdout.take().unwrap()).lines();
    assert!(
        lines.any(|l| l.is_ok_and(|l| l.contains(IN_SPAN))),
        "the child never entered its write span"
    );

    let ran = run(move || text(c.run(0.0)));
    still_waiting(&ran, "a cycle, while another process is in a write span,");
    drop(stdin);
    finishes(
        &ran,
        "the cycle, after the other process left its write span,",
    )
    .unwrap();
    drop(lines);
    assert!(child.wait().unwrap().success(), "child process");
}
