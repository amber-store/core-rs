//! Ported Go tests of the gate between write spans and sweeps
//! (`gate_test.go`).
//!
//! Two stores on one directory stand in for two processes: `gc.lock` is a
//! flock, which belongs to the open file.

use std::sync::{Arc, mpsc};
use std::thread;
use std::time::{Duration, Instant};

use tempfile::TempDir;

use super::gc_tests::compact_store;
use super::store_tests::obj_seq;
use super::testutil::*;
use super::{Error, Options, Store};

/// A wait that must end.
const GATE_WAIT: Duration = Duration::from_secs(10);
/// How long "still waiting" is watched.
const GATE_QUIET: Duration = Duration::from_millis(150);

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
            rx.recv_timeout(GATE_QUIET),
            Err(mpsc::RecvTimeoutError::Timeout)
        ),
        "{what} did not wait"
    );
}

fn finishes<T>(rx: &mpsc::Receiver<T>, what: &str) -> T {
    rx.recv_timeout(GATE_WAIT)
        .unwrap_or_else(|_| panic!("{what} is still waiting"))
}

fn text<T>(r: Result<T, Error>) -> Result<(), String> {
    r.map(drop).map_err(|e| e.to_string())
}

fn nosync() -> Options {
    Options::new().sync(false)
}

fn two_stores() -> (TempDir, Arc<Store>, Arc<Store>) {
    let dir = TempDir::new().unwrap();
    let a = Arc::new(Store::open_with(dir.path(), nosync()).unwrap());
    let b = Arc::new(Store::open_with(dir.path(), nosync()).unwrap());
    (dir, a, b)
}

/// Takes the store for a sweep and lets go at once.
fn sweep_once(s: &Store) -> Result<(), String> {
    text(s.begin_sweep(&|| false))
}

#[test]
fn write_span_blocks_a_foreign_sweep() {
    let (_dir, a, b) = two_stores();
    let span = a.begin_write().unwrap();
    let swept = run(move || sweep_once(&b));
    still_waiting(&swept, "a sweep, while another store is in a write span,");
    drop(span);
    finishes(&swept, "the sweep, after the write span ended,").unwrap();
}

#[test]
fn sweep_blocks_foreign_writes() {
    let (_dir, a, b) = two_stores();
    let objs = test_objects(3);
    let sweep = a.begin_sweep(&|| false).unwrap();

    let (b1, b2, b3) = (b.clone(), b.clone(), b.clone());
    let (o0, rest) = (objs[0].clone(), objs[1..].to_vec());
    let span = run(move || text(b1.begin_write()));
    let put = run(move || text(b2.put(o0.key, &o0.data)));
    let batch = run(move || text(b3.write_batch(obj_seq(&rest, None))));
    still_waiting(&span, "a write span, while another store sweeps,");
    still_waiting(&put, "a put, while another store sweeps,");
    still_waiting(&batch, "a write_batch, while another store sweeps,");
    drop(sweep);
    finishes(&span, "the write span, after the sweep ended,").unwrap();
    finishes(&put, "the put, after the sweep ended,").unwrap();
    finishes(&batch, "the write_batch, after the sweep ended,").unwrap();
    want_objects(&a, &objs);
}

// Writers in the sweeping store's own process are not held up by the file
// lock: the write barrier and the collector's reference lock deal with them.
#[test]
fn local_writes_run_under_a_local_sweep_lock() {
    let (_dir, a, _b) = two_stores();
    let o = test_objects(1).remove(0);
    let sweep = a.begin_sweep(&|| false).unwrap();
    let writer = a.clone();
    let put = run(move || text(writer.put(o.key, &o.data)));
    finishes(&put, "a put in the store that holds the sweep lock").unwrap();
    drop(sweep);
}

#[test]
fn nested_write_spans_count() {
    let (_dir, a, b) = two_stores();
    let (outer, inner) = (a.begin_write().unwrap(), a.begin_write().unwrap());
    let swept = run(move || sweep_once(&b));
    drop(inner);
    still_waiting(&swept, "a sweep, with one of two nested spans still open,");
    drop(outer);
    finishes(&swept, "the sweep, after both spans ended,").unwrap();
}

/// A sweep that gives up, on its own thread and bounded: one that ignored its
/// cancellation would otherwise hang the suite instead of failing it.
fn gives_up(sweeper: Arc<Store>, what: &str) {
    let gave_up = run(move || {
        let deadline = Instant::now() + Duration::from_millis(100);
        text(sweeper.begin_sweep(&move || Instant::now() >= deadline))
    });
    let err = finishes(&gave_up, what).unwrap_err();
    assert!(err.contains("canceled while waiting"), "{err}");
}

/// Waiting for another store's span is a poll of the file lock.
#[test]
fn begin_sweep_honours_cancellation() {
    let (_dir, a, b) = two_stores();
    let _span = a.begin_write().unwrap();
    gives_up(b.clone(), "a cancelled sweep, behind another store's span,");
    // Giving up must not leave the store's own writers held.
    let o = test_objects(1).remove(0);
    let put = run(move || text(b.put(o.key, &o.data)));
    finishes(&put, "a put after a sweep gave up").unwrap();
}

/// Waiting for a span of the sweep's own store is a wait on the condition
/// variable: another code path, with `blocked` to take back.
#[test]
fn begin_sweep_honours_cancellation_behind_a_local_span() {
    let (_dir, a, _b) = two_stores();
    let span = a.begin_write().unwrap();
    gives_up(
        a.clone(),
        "a cancelled sweep, behind a span of its own store,",
    );
    let o = test_objects(1).remove(0);
    let writer = a.clone();
    let put = run(move || text(writer.put(o.key, &o.data)));
    finishes(
        &put,
        "a put after a sweep gave up, its store's span still open,",
    )
    .unwrap();
    drop(span);
}

/// The shared file lock is never turned into an exclusive one in place, which
/// `flock` does not promise to do atomically: a sweep waits for its own
/// store's spans first, and until then other stores' spans still get in.
#[test]
fn a_waiting_local_sweep_leaves_the_shared_lock_alone() {
    let (_dir, a, b) = two_stores();
    let span = a.begin_write().unwrap(); // a holds gc.lock shared
    let sweeper = a.clone();
    let swept = run(move || sweep_once(&sweeper));
    still_waiting(&swept, "a sweep, while a span of its own store is open,");
    let other = run(move || text(b.begin_write()));
    finishes(
        &other,
        "another store's write span, while the first store's sweep waits for its own span,",
    )
    .unwrap();
    drop(span);
    finishes(&swept, "the sweep, after the span ended,").unwrap();
}

/// Spans that joined a local sweep hold no file lock of their own, so the
/// sweep's lock must outlive them: nor can it be turned into a shared one.
#[test]
fn the_end_of_a_sweep_waits_for_spans_that_ran_under_it() {
    let (_dir, a, b) = two_stores();
    thread::scope(|sc| {
        let sweep = a.begin_sweep(&|| false).unwrap();
        let span = a.begin_write().unwrap(); // joins the sweep
        let (tx, ended) = mpsc::channel();
        sc.spawn(move || {
            drop(sweep);
            let _ = tx.send(());
        });
        still_waiting(
            &ended,
            "the end of a sweep, with a span that ran under it still open,",
        );
        let other = b.clone();
        let swept = run(move || sweep_once(&other));
        still_waiting(
            &swept,
            "another store's sweep, under a span that joined a local sweep,",
        );
        drop(span);
        finishes(&ended, "the end of the sweep, after the span ended,");
        finishes(&swept, "the other store's sweep, after that,").unwrap();
    });
}

/// Inside a collector's sweep the gate is already held, so `compact`'s own
/// pause is all that keeps it apart from writes that bypass the collector.
#[test]
fn compact_inside_a_sweep_waits_for_local_write_spans() {
    let (_dir, s, objs) = compact_store();
    let s = Arc::new(s);
    let sweep = s.begin_sweep(&|| false).unwrap();
    let span = s.begin_write().unwrap();
    let (sweeper, live) = (s.clone(), live_at(&objs, &[0, 2, 4]));
    let compacted = run(move || text(sweeper.compact(live, sweep_opts())));
    still_waiting(
        &compacted,
        "compact inside a sweep, while a write span of its own store is open,",
    );
    drop(span);
    finishes(&compacted, "compact, after the write span ended,").unwrap();
    drop(sweep);
}

/// A panic in the refresh, under the gate's busy step, must leave neither
/// the store's later spans and sweeps waiting for ever nor the file lock
/// held against other stores (in Go the panic would take the process down).
#[test]
fn a_panic_in_the_refresh_does_not_hold_the_gate() {
    let (_dir, a, b) = two_stores();
    for exclusive in [false, true] {
        let store = a.clone();
        let panicked = run(move || {
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                if exclusive {
                    let _ = store
                        .gate
                        .begin_exclusive(None, || panic!("in the refresh"));
                } else {
                    let _ = store.gate.begin_shared(|| panic!("in the refresh"));
                }
            }))
            .is_err()
        });
        assert!(
            finishes(&panicked, "the step whose refresh panics"),
            "the refresh did not run (exclusive: {exclusive})"
        );
        let own = a.clone();
        let span = run(move || text(own.begin_write()));
        finishes(&span, "a span of the same store, after a refresh panicked,").unwrap();
        let other = b.clone();
        let swept = run(move || sweep_once(&other));
        finishes(&swept, "another store's sweep, after a refresh panicked,").unwrap();
    }
}

#[test]
fn generation_moves_on_sweep_and_wipe() {
    let (_dir, a, _b) = two_stores();
    let start = a.gate.generation().unwrap();
    sweep_once(&a).unwrap();
    assert_eq!(a.gate.generation().unwrap(), start + 1, "after a sweep");
    a.wipe().unwrap();
    assert_eq!(a.gate.generation().unwrap(), start + 2, "after a wipe");
}

/// What became of `doomed` after a store wrote it against a view that another
/// store's pass may have made stale: with the refreshes disabled the object
/// must be lost, or the test proves nothing.
fn check_doomed(never_refresh: bool, got: Result<Vec<u8>, Error>) {
    match (never_refresh, got) {
        (false, Ok(_)) => {}
        (false, Err(e)) => {
            panic!("the object was skipped as a duplicate of a record that had been reaped: {e}")
        }
        (true, Err(e)) if e.is_not_found() => {}
        (true, other) => panic!(
            "get = {:?}: without the refresh the object should be lost, or this test proves nothing",
            other.map(|b| b.len()).map_err(|e| e.to_string())
        ),
    }
}

// A store that mapped a segment keeps the mapping when another store reaps
// it. Its duplicate check would then find an object that is gone, skip the
// write, and lose it. The generation makes it look again first. With the
// check disabled the object is lost, which shows that this test bites.
#[test]
fn stale_writer_refreshes_before_dedup() {
    for never_refresh in [false, true] {
        let (dir, a, objs) = compact_store(); // objs[0..2] and objs[2..4] sealed, objs[4] active
        let b = Store::open_with(dir.path(), nosync()).unwrap();
        b.gate.set_never_refresh(never_refresh);
        let doomed = &objs[1];
        assert!(b.has(doomed.key).unwrap());
        a.compact(live_at(&objs, &[0, 2, 3, 4]), sweep_opts())
            .unwrap();
        // As an ingest of a tree that still contains the object would.
        b.put(doomed.key, &doomed.data).unwrap();
        check_doomed(
            never_refresh,
            Store::open(dir.path()).unwrap().get(doomed.key),
        );
    }
}

#[test]
fn wipe_waits_for_a_foreign_write_span() {
    let (dir, a, b) = two_stores();
    let o = test_objects(1).remove(0);
    b.put(o.key, &o.data).unwrap();
    b.close().unwrap(); // leaves an idle segment for the wipe to take
    let span = a.begin_write().unwrap();
    let c = Store::open_with(dir.path(), nosync()).unwrap();
    let wiped = run(move || text(c.wipe()));
    still_waiting(&wiped, "a wipe, while another store is in a write span,");
    drop(span);
    finishes(&wiped, "the wipe, after the write span ended,").unwrap();
    // Asked of a store that opens now: a's view, brought up to date by its
    // span, still reads the wiped segment through the file it holds open, as
    // any view reads a reaped one until it next looks at the directory.
    assert!(
        !Store::open(dir.path()).unwrap().has(o.key).unwrap(),
        "the wiped object is still there"
    );
}

// compact also waits for this store's own writes that bypass the collector:
// their duplicate check must not race the removal of a segment.
#[test]
fn compact_waits_for_local_write_spans() {
    let (_dir, s, objs) = compact_store();
    let s = Arc::new(s);
    let span = s.begin_write().unwrap();
    let (sweeper, live) = (s.clone(), live_at(&objs, &[0, 2, 4]));
    let compacted = run(move || text(sweeper.compact(live, sweep_opts())));
    still_waiting(
        &compacted,
        "compact, while a write span of its own store is open,",
    );
    drop(span);
    finishes(&compacted, "compact, after the write span ended,").unwrap();
}

// A store that sweeps again and again must not starve a writer in another
// process, which polls for the lock: between two sweeps the lock is free for
// microseconds. Without the yield the writer below gets in only by luck.
#[test]
fn back_to_back_sweeps_yield_to_a_waiting_writer() {
    let (_dir, a, b) = two_stores();
    let o = test_objects(1).remove(0);
    let first = a.begin_sweep(&|| false).unwrap();
    let put = run(move || text(b.put(o.key, &o.data)));
    still_waiting(&put, "a put, while another store sweeps,");
    drop(first);

    let mut sweeps = 0;
    let mut done = None;
    while sweeps < 200 {
        match put.try_recv() {
            Ok(res) => {
                done = Some(res);
                break;
            }
            Err(mpsc::TryRecvError::Empty) => {
                sweep_once(&a).unwrap();
                sweeps += 1;
            }
            Err(mpsc::TryRecvError::Disconnected) => panic!("the writer vanished"),
        }
    }
    done.unwrap_or_else(|| finishes(&put, "the put")).unwrap();
    assert!(
        sweeps <= 20,
        "the writer got in only after {sweeps} further sweeps"
    );
}

// Stores that sweep in turn must each move the generation past what the file
// holds, not past what they remember from their own last look at it: a store
// that remembered an older value would write the current one again, and a
// writer that had seen it already would not look at the directory again.
#[test]
fn every_sweep_moves_the_generation() {
    let (dir, a, objs) = compact_store();
    let b = Store::open_with(dir.path(), nosync()).unwrap(); // opens before any sweep
    sweep_once(&a).unwrap();
    let w = Store::open_with(dir.path(), nosync()).unwrap(); // opens after a's sweep
    // w's first write brings its view, and the generation it holds, up to date.
    put_all(&w, &[blob_obj(b"w's first write")]);
    let doomed = &objs[1];
    assert!(w.has(doomed.key).unwrap());
    let before = w.gate.generation().unwrap();
    b.compact(live_at(&objs, &[0, 2, 3, 4]), sweep_opts())
        .unwrap();
    let after = w.gate.generation().unwrap();
    assert!(
        after > before,
        "generation {after} after a second store swept, {before} before: every sweep must move it"
    );
    w.put(doomed.key, &doomed.data).unwrap();
    check_doomed(false, Store::open(dir.path()).unwrap().get(doomed.key));
}

// A write inside an open span — a put inside a begin_write bracket, or a
// second bracket — must not wait for a sweep of the same store that is
// waiting for the bracket to close: each would wait for the other.
#[test]
fn writes_inside_a_span_pass_a_waiting_sweep() {
    for which in ["compact", "wipe", "begin_sweep"] {
        let (_dir, s, objs) = compact_store();
        let s = Arc::new(s);
        let span = s.begin_write().unwrap(); // dropped, letting everybody go, when the test fails half way
        let (sweeper, live) = (s.clone(), live_at(&objs, &[0, 2, 4]));
        let swept = run(move || match which {
            "compact" => text(sweeper.compact(live, sweep_opts())),
            "wipe" => text(sweeper.wipe()),
            _ => sweep_once(&sweeper),
        });
        still_waiting(
            &swept,
            &format!("{which}, while a write span of its own store is open,"),
        );

        let (writer, o) = (s.clone(), blob_obj(b"written inside the span"));
        let inside = run(move || {
            text(writer.put(o.key, &o.data))?;
            text(writer.begin_write())
        });
        finishes(
            &inside,
            &format!("a write inside the open span, with {which} waiting for the span,"),
        )
        .unwrap();
        drop(span);
        finishes(&swept, &format!("{which}, after the span ended,")).unwrap();
    }
}

// A store that opens while another is in the middle of a sweep lists a
// directory that is about to change, and no generation it could read then
// tells it so reliably. Its first write span therefore looks again. With the
// refresh disabled the object is lost, which shows that this test bites.
#[test]
fn store_opened_during_a_sweep_looks_again_before_its_first_write() {
    for never_refresh in [false, true] {
        let (dir, a, objs) = compact_store();
        let sweep = a.begin_sweep(&|| false).unwrap();
        let x = Store::open_with(dir.path(), nosync()).unwrap(); // in the middle of a's sweep
        x.gate.set_never_refresh(never_refresh);
        let doomed = &objs[1];
        assert!(x.has(doomed.key).unwrap());
        a.compact(live_at(&objs, &[0, 2, 3, 4]), sweep_opts())
            .unwrap(); // nested in the sweep
        drop(sweep);
        x.put(doomed.key, &doomed.data).unwrap();
        check_doomed(
            never_refresh,
            Store::open(dir.path()).unwrap().get(doomed.key),
        );
    }
}

// The generation moves when the sweep begins, not when it ends: a sweeper
// that dies after deleting segments has still told every other store.
#[test]
fn generation_moves_before_anything_is_deleted() {
    let (_dir, a, _b) = two_stores();
    let start = a.gate.generation().unwrap();
    let _sweep = a.begin_sweep(&|| false).unwrap();
    assert_eq!(a.gate.generation().unwrap(), start + 1, "inside a sweep");
}
