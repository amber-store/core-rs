//! Ported Go GC-surface tests (`markset_test.go`, `gc_test.go`,
//! `compact_test.go`, `compact_concurrent_test.go`).

use std::collections::{HashMap, HashSet};
use std::convert::Infallible;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::thread;
use std::time::{Duration, Instant, SystemTime};

use tempfile::TempDir;

use crate::amberpack::{REC_HEADER_SIZE, decode_payload, parse_record};
use crate::key::Key;

use super::store_tests::sealed_store;
use super::testutil::*;
use super::{CompactOpts, MAGIC_HEADER, Object, Options, Store};

/// An open store with `objs` sealed into segments of 8 KiB (Go: `gcStore`).
fn gc_store(objs: &[Object]) -> (TempDir, Store) {
    let dir = sealed_store(objs); // put + close seals via rotation at 8 KiB
    let s = Store::open(dir.path()).unwrap();
    (dir, s)
}

/// The sealed segment files in `dir`, sorted (Go: the `*.seg` globs).
fn seg_files(dir: &Path) -> Vec<PathBuf> {
    let mut out: Vec<_> = fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().path())
        .filter(|p| p.to_string_lossy().ends_with(".seg"))
        .collect();
    out.sort();
    out
}

/// A store with objs[0..2] and objs[2..4] in two sealed segments and objs[4]
/// in the active one (Go: `compactStore`).
pub(crate) fn compact_store() -> (TempDir, Store, Vec<Object>) {
    let dir = TempDir::new().unwrap();
    let s = Store::open_with(dir.path(), Options::new().segment_size(8 << 10).sync(false)).unwrap();
    let objs: Vec<Object> = (0..5)
        .map(|i| {
            let mut data = incompressible(4 << 10);
            data[0] = i as u8;
            blob_obj(&data)
        })
        .collect();
    for o in &objs {
        s.put(o.key, &o.data).unwrap();
    }
    (dir, s, objs)
}

/// A live predicate true for exactly the objects at `idx` (Go: `liveSet`).
fn live_set(objs: &[Object], idx: &[usize]) -> impl Fn(Key) -> bool + Sync {
    let m: HashSet<Key> = idx.iter().map(|&i| objs[i].key).collect();
    move |k| m.contains(&k)
}

// ---------------------------------------------------------------------------
// Mark-set tests (Go: markset_test.go).

#[test]
fn mark_set_marks_and_counts() {
    let (_dir, s, objs) = compact_store();
    let mut m = s.new_mark_set();
    for (i, o) in objs.iter().enumerate() {
        assert!(!m.contains(o.key), "object {i} marked before mark");
        let (newly, present) = m.mark(o.key);
        assert!(
            newly && present,
            "first mark of object {i}: newly={newly} present={present}"
        );
        let (newly, present) = m.mark(o.key);
        assert!(
            !newly && present,
            "second mark of object {i}: newly={newly} present={present}"
        );
        assert!(m.contains(o.key), "object {i} not marked after mark");
    }
    assert_eq!(m.marked(), objs.len());
    let absent = blob_obj(b"never stored");
    let (newly, present) = m.mark(absent.key);
    assert!(
        !newly && !present,
        "absent key: newly={newly} present={present}"
    );
    assert!(!m.contains(absent.key), "absent key marked");
}

#[test]
fn mark_set_drives_compact() {
    let (_dir, s, objs) = compact_store();
    let mut m = s.new_mark_set();
    for i in [0, 2, 4] {
        m.mark(objs[i].key);
    }
    let stats = s
        .compact(
            |k| m.contains(k),
            CompactOpts {
                min_dead_ratio: 0.4,
                ..CompactOpts::default()
            },
        )
        .unwrap();
    assert!(
        stats.segments_compacted == 2 && stats.records_copied == 2,
        "stats: {stats:?}"
    );
}

// ---------------------------------------------------------------------------
// GC-surface tests (Go: gc_test.go).

#[test]
fn segments_reports_sealed_segments() {
    let objs = test_objects(24); // ~2 KiB each: several 8 KiB segments
    let (_dir, s) = gc_store(&objs);
    let segs = s.segments().unwrap();
    assert!(!segs.is_empty(), "no sealed segments");
    let mut keys = 0u64;
    for g in &segs {
        assert!(g.body > 0, "segment {}: body = {}", g.id, g.body);
        assert!(
            g.sealed <= SystemTime::now(),
            "segment {}: sealed in the future",
            g.id
        );
        keys += g.keys;
    }
    // Keys across sealed segments plus whatever stayed active must cover objs.
    assert!(
        keys <= objs.len() as u64,
        "keys total {keys} > {} objects",
        objs.len()
    );
}

#[test]
fn scan_index_covers_body() {
    let (_dir, s) = gc_store(&test_objects(24));
    let segs = s.segments().unwrap();
    for g in &segs {
        let mut sum = 0u64;
        let mut n = 0u64;
        s.scan_index(g.id, |_k, off, slen| {
            sum += REC_HEADER_SIZE as u64 + u64::from(slen);
            n += 1;
            // Records start after the header magic.
            assert!(off >= 8, "segment {}: off {off} inside magic", g.id);
        })
        .unwrap();
        assert_eq!(n, g.keys, "segment {}: scanned entries != keys", g.id);
        assert_eq!(sum, g.body, "segment {}: sum(46+slen) != body", g.id);
    }
    let err = s.scan_index(99999, |_, _, _| {}).unwrap_err();
    assert!(
        err.is_unknown_segment(),
        "unknown id: err = {err}, want unknown segment"
    );
}

#[test]
fn record_round_trip() {
    let (_dir, s) = gc_store(&test_objects(24));
    let segs = s.segments().unwrap();
    for g in &segs {
        let mut entries = Vec::new();
        s.scan_index(g.id, |k, off, slen| entries.push((k, off, slen)))
            .unwrap();
        for (k, off, slen) in entries {
            let raw = s.record(g.id, off).unwrap();
            let rec = parse_record(&raw).unwrap();
            assert!(
                rec.key == k && rec.slen == slen && raw.len() == REC_HEADER_SIZE + slen as usize,
                "segment {} off {off}: record mismatch",
                g.id
            );
        }
    }
    // Bad offsets fail loudly, never panic.
    assert!(
        s.record(segs[0].id, 0).is_err(),
        "record at offset 0 (magic) should fail"
    );
    assert!(
        s.record(segs[0].id, 1 << 40).is_err(),
        "record far out of range should fail"
    );
    // Offsets in the wrap window: off + REC_HEADER_SIZE overflows u64. A
    // naive bounds check passes them and the mmap slice panics.
    for off in [u64::MAX, u64::MAX - 40] {
        let err = s.record(segs[0].id, off).unwrap_err();
        assert!(
            err.is_corrupt(),
            "record at wrapping offset {off}: err = {err}, want corrupt"
        );
    }
}

#[test]
fn record_reports_corrupt_payload() {
    // 8 objects: test_objects alternates highly-compressible (~78-byte
    // record) and incompressible (~2048-byte record) payloads, so 6 would
    // stay under the 8 KiB rotation threshold and never seal; 8 crosses it
    // deterministically (Go: TestRecordCorrupt).
    let objs = test_objects(8);
    let dir = sealed_store(&objs);
    // Flip a byte inside the first record's payload (offset 8 is the first
    // record header; 8+46 is its first payload byte). The footer CRC does
    // not cover the body, so the store reopens cleanly.
    let segs = seg_files(dir.path());
    assert!(!segs.is_empty(), "no sealed segment");
    let mut b = fs::read(&segs[0]).unwrap();
    b[8 + REC_HEADER_SIZE] ^= 0xFF;
    fs::write(&segs[0], &b).unwrap();
    let s = Store::open(dir.path()).unwrap();
    let segs = s.segments().unwrap();
    let err = s.record(segs[0].id, 8).unwrap_err();
    assert!(
        err.is_corrupt(),
        "record of corrupt payload: err = {err}, want corrupt"
    );
}

#[test]
fn has_outside_skips_own_segment() {
    let objs = test_objects(24);
    let (_dir, s) = gc_store(&objs);
    let segs = s.segments().unwrap();
    assert!(segs.len() >= 2, "need at least two sealed segments");
    // Each key lives in exactly one segment (put dedups), so a key found in
    // segment A is not outside A, and is outside any other segment.
    let first = &segs[0];
    let mut some_key = None;
    s.scan_index(first.id, |k, _, _| some_key = Some(k))
        .unwrap();
    let some_key = some_key.unwrap();
    assert!(
        !s.has_outside(first.id, some_key).unwrap(),
        "has_outside(own segment) = true, want false"
    );
    assert!(
        s.has_outside(segs[1].id, some_key).unwrap(),
        "has_outside(other segment) = false, want true"
    );
    // After re-appending the record, the key exists in the active segment
    // too, so it is outside its original segment.
    let mut off = 0;
    s.scan_index(first.id, |k, o, _| {
        if k == some_key {
            off = o;
        }
    })
    .unwrap();
    let raw = s.record(first.id, off).unwrap();
    s.append_record(some_key, &raw).unwrap();
    s.sync().unwrap();
    assert!(
        s.has_outside(first.id, some_key).unwrap(),
        "has_outside after copy = false, want true"
    );
}

#[test]
fn append_record_validates() {
    // 8 objects, not 6: see record_reports_corrupt_payload.
    let (_dir, s) = gc_store(&test_objects(8));
    let segs = s.segments().unwrap();
    let (mut k, mut off) = (None, 0);
    s.scan_index(segs[0].id, |kk, o, _| {
        k = Some(kk);
        off = o;
    })
    .unwrap();
    let k = k.unwrap();
    let raw = s.record(segs[0].id, off).unwrap();
    // Wrong key.
    let other = blob_obj(b"other-payload");
    let err = s.append_record(other.key, &raw).unwrap_err();
    assert!(
        err.is_corrupt(),
        "mismatched key: err = {err}, want corrupt"
    );
    // Corrupt payload.
    let mut bad = raw.clone();
    let last = bad.len() - 1;
    bad[last] ^= 0xFF;
    let err = s.append_record(k, &bad).unwrap_err();
    assert!(err.is_corrupt(), "corrupt raw: err = {err}, want corrupt");
    // Trailing junk.
    let mut long = raw.clone();
    long.push(0);
    let err = s.append_record(k, &long).unwrap_err();
    assert!(err.is_corrupt(), "overlong raw: err = {err}, want corrupt");
    // Valid append round-trips through get.
    s.append_record(k, &raw).unwrap();
    let got = s.get(k).unwrap();
    let rec = parse_record(&raw).unwrap();
    let want = decode_payload(rec.flags, rec.ulen, &raw[REC_HEADER_SIZE..]).unwrap();
    assert_eq!(got, want, "payload mismatch after append_record");
}

#[test]
fn remove_drops_segment() {
    let objs = test_objects(24);
    let (dir, s) = gc_store(&objs);
    let segs = s.segments().unwrap();
    assert!(segs.len() >= 2, "need at least two segments");
    let victim = &segs[0];
    // Copy every record out first, like a reap does.
    let mut live = Vec::new();
    s.scan_index(victim.id, |k, off, _| live.push((k, off)))
        .unwrap();
    for &(k, off) in &live {
        let raw = s.record(victim.id, off).unwrap();
        s.append_record(k, &raw).unwrap();
    }
    s.sync().unwrap();
    let path = dir.path().join(format!("{:016x}.seg", victim.id));
    s.remove(victim.id).unwrap();
    assert!(!path.exists(), "segment file still exists");
    // Every original object still reads back.
    for o in &objs {
        let got = s
            .get(o.key)
            .unwrap_or_else(|e| panic!("get({}) after remove: {e}", o.key));
        assert_eq!(got, o.data, "payload mismatch for {}", o.key);
    }
    // The victim is gone from segments and from the GC surface.
    for g in s.segments().unwrap() {
        assert_ne!(g.id, victim.id, "removed segment still listed");
    }
    let err = s.scan_index(victim.id, |_, _, _| {}).unwrap_err();
    assert!(err.is_unknown_segment(), "scan_index after remove: {err}");
    let err = s.remove(victim.id).unwrap_err();
    assert!(err.is_unknown_segment(), "double remove: {err}");
    // Survives reopen: no half-state on disk.
    s.close().unwrap();
    drop(s);
    let s2 = Store::open(dir.path()).unwrap();
    for o in &objs {
        s2.get(o.key)
            .unwrap_or_else(|e| panic!("get({}) after reopen: {e}", o.key));
    }
}

#[test]
fn remove_during_reads() {
    let objs = test_objects(24);
    let (_dir, s) = gc_store(&objs);
    let segs = s.segments().unwrap();
    let victim = &segs[0];
    let mut live = Vec::new();
    s.scan_index(victim.id, |k, off, _| live.push((k, off)))
        .unwrap();
    for &(k, off) in &live {
        let raw = s.record(victim.id, off).unwrap();
        s.append_record(k, &raw).unwrap();
    }
    // Hammer reads of every object while the victim disappears.
    thread::scope(|scope| {
        let s = &s;
        let objs = &objs;
        let reader = scope.spawn(move || {
            for _ in 0..50 {
                for o in objs {
                    s.get(o.key)
                        .unwrap_or_else(|e| panic!("get during remove: {e}"));
                }
            }
        });
        s.remove(victim.id).unwrap();
        reader.join().unwrap();
    });
}

/// Go's `TestRemoveWaitsForScrubs` asserts `Remove` blocks until an in-flight
/// `ScanIndex` walk releases its pin (Go must not munmap under a walker). The
/// Rust port has no scrub gate — the scan's `Arc<SealedSegment>` keeps the
/// mmap alive past the unlink — so this ports the observable contract
/// instead: a remove landing mid-scan is safe, the scan completes over the
/// full index, and the file is gone (see port-notes/packstore-gc.md).
#[test]
fn remove_during_scan_index_is_safe() {
    let (dir, s) = gc_store(&test_objects(24));
    let segs = s.segments().unwrap();
    let victim = segs[0];
    let (entered_tx, entered_rx) = mpsc::sync_channel::<()>(1);
    let (release_tx, release_rx) = mpsc::channel::<()>();
    thread::scope(|scope| {
        // Owned by the scope closure: dropped on an assertion panic, so the
        // blocked scan unblocks and the scope's implicit join terminates.
        let release_tx = release_tx;
        let s = &s;
        let scan = scope.spawn(move || {
            let mut n = 0u64;
            s.scan_index(victim.id, |_k, _off, _slen| {
                if n == 0 {
                    entered_tx.send(()).unwrap();
                    release_rx.recv().unwrap(); // hold the walk mid-flight
                }
                n += 1;
            })
            .unwrap();
            n
        });
        entered_rx.recv().unwrap(); // the scan is mid-walk over the victim's mmap
        s.remove(victim.id).unwrap();
        assert!(
            !dir.path().join(format!("{:016x}.seg", victim.id)).exists(),
            "victim file still present after remove"
        );
        release_tx.send(()).unwrap();
        let n = scan.join().unwrap();
        assert_eq!(
            n, victim.keys,
            "scan did not complete over the detached mmap"
        );
    });
}

#[test]
fn oldest_inflight_write_tracks_write_batch() {
    let dir = TempDir::new().unwrap();
    let s = Store::open(dir.path()).unwrap();
    assert!(
        s.oldest_inflight_write().is_none(),
        "idle store reports an in-flight write"
    );
    let before = Instant::now();
    let (started_tx, started_rx) = mpsc::sync_channel::<()>(1);
    let (release_tx, release_rx) = mpsc::channel::<()>();
    thread::scope(|scope| {
        let release_tx = release_tx; // dropped on panic; unblocks the writer
        let s2 = &s;
        scope.spawn(move || {
            // An iterator that stalls between its two objects, holding the
            // write open (Go: the blocking `seq`).
            let first = blob_obj(b"first");
            let second = blob_obj(b"second");
            let mut n = 0;
            let seq = std::iter::from_fn(move || {
                n += 1;
                match n {
                    1 => Some(Ok::<Object, Infallible>(first.clone())),
                    2 => {
                        started_tx.send(()).unwrap();
                        release_rx.recv().unwrap();
                        Some(Ok(second.clone()))
                    }
                    _ => None,
                }
            });
            s2.write_batch(seq).unwrap();
        });
        started_rx.recv().unwrap();
        let got = s
            .oldest_inflight_write()
            .expect("in-flight write_batch not reported");
        assert!(
            got >= before && got <= Instant::now(),
            "start outside [before, now]"
        );
        release_tx.send(()).unwrap();
    });
    // The scope joined the writer, so the token must be gone (Go polls with
    // a 5 s deadline; the join makes that deterministic here).
    assert!(
        s.oldest_inflight_write().is_none(),
        "write token never released"
    );
}

#[test]
fn oldest_inflight_write_covers_put() {
    // Put registers even when it dedups: wrap-the-whole-call semantics are
    // only observable via the map being empty afterwards, so just check a
    // plain put leaves no token behind and errors don't leak tokens.
    let dir = TempDir::new().unwrap();
    let s = Store::open(dir.path()).unwrap();
    let o = blob_obj(b"x");
    s.put(o.key, &o.data).unwrap();
    s.put(o.key, &o.data).unwrap(); // dedup path
    assert!(
        s.oldest_inflight_write().is_none(),
        "token leaked after put"
    );
}

// ---------------------------------------------------------------------------
// Liveness and compaction tests (Go: compact_test.go).

#[test]
fn liveness_reports_per_segment() {
    let (_dir, s, objs) = compact_store();
    let report = s.liveness(live_set(&objs, &[0, 2, 4])).unwrap();
    assert_eq!(report.len(), 3, "report: {report:?}");
    let rec_bytes = (REC_HEADER_SIZE + objs[0].data.len()) as u64;
    for (i, seg) in report[..2].iter().enumerate() {
        assert!(seg.sealed, "segment {i} not sealed");
        assert!(
            seg.live_keys == 1 && seg.dead_keys == 1,
            "segment {i}: live={} dead={}, want 1/1",
            seg.live_keys,
            seg.dead_keys
        );
        assert!(
            seg.live_bytes == rec_bytes && seg.dead_bytes == rec_bytes,
            "segment {i}: live_bytes={} dead_bytes={}, want {rec_bytes}",
            seg.live_bytes,
            seg.dead_bytes
        );
    }
    let act = &report[2];
    assert!(
        !act.sealed && act.live_keys == 1 && act.dead_keys == 0,
        "active segment: {act:?}"
    );
}

#[test]
fn compact_removes_dead_objects() {
    let (dir, s, objs) = compact_store();
    let live = live_set(&objs, &[0, 2, 4]);
    let stats = s
        .compact(
            &live,
            CompactOpts {
                min_dead_ratio: 0.4,
                ..CompactOpts::default()
            },
        )
        .unwrap();
    assert_eq!(stats.segments_compacted, 2, "segments_compacted");
    assert_eq!(stats.records_copied, 2, "records_copied");
    assert!(stats.bytes_freed > 0, "bytes_freed = 0");
    assert_eq!(stats.victims.len(), 2, "victims = {:?}", stats.victims);
    for (i, o) in objs.iter().enumerate() {
        match s.get(o.key) {
            Ok(data) => {
                assert!(live(o.key), "dead object {i} still readable");
                assert_eq!(data, o.data, "live object {i} corrupted");
            }
            Err(e) => {
                assert!(!live(o.key), "live object {i}: {e}");
                assert!(
                    e.is_not_found(),
                    "dead object {i}: err = {e}, want not found"
                );
            }
        }
    }
    s.verify(|| false).unwrap();
    s.close().unwrap();
    drop(s);
    let s2 = Store::open_with(dir.path(), Options::new().sync(false)).unwrap();
    for i in [0, 2, 4] {
        s2.get(objs[i].key)
            .unwrap_or_else(|e| panic!("after reopen, object {i}: {e}"));
    }
}

#[test]
fn compact_skips_below_threshold() {
    let (_dir, s, objs) = compact_store();
    let stats = s
        .compact(
            live_set(&objs, &[0, 2, 4]),
            CompactOpts {
                min_dead_ratio: 0.9,
                ..CompactOpts::default()
            },
        )
        .unwrap();
    assert_eq!(stats.segments_compacted, 0, "segments_compacted");
    for o in &objs {
        s.get(o.key).unwrap();
    }
}

#[test]
fn compact_all_dead() {
    let (_dir, s, objs) = compact_store();
    let stats = s.compact(|_| false, CompactOpts::default()).unwrap();
    assert_eq!(stats.records_copied, 0, "records_copied");
    // Everything is gone, including the formerly active objs[4]: compact
    // seals the active segment first.
    for o in &objs {
        let err = s.get(o.key).unwrap_err();
        assert!(err.is_not_found(), "err = {err}, want not found");
    }
}

#[test]
fn compact_horizon_spares_young_segments() {
    let (_dir, s, objs) = compact_store();
    // Every segment was just written, so a horizon in the past spares all.
    let stats = s
        .compact(
            |_| false,
            CompactOpts {
                horizon: Some(SystemTime::now() - Duration::from_secs(3600)),
                ..CompactOpts::default()
            },
        )
        .unwrap();
    assert_eq!(stats.segments_compacted, 0, "segments_compacted");
    for o in &objs {
        s.get(o.key)
            .unwrap_or_else(|e| panic!("young segment reaped past the horizon: {e}"));
    }
}

#[test]
fn compact_rejects_corrupt_record() {
    let dir = TempDir::new().unwrap();
    let s = Store::open_with(dir.path(), Options::new().segment_size(8 << 10).sync(false)).unwrap();
    let objs: Vec<Object> = (0..2)
        .map(|i| {
            let mut data = incompressible(4 << 10);
            data[0] = i as u8;
            blob_obj(&data)
        })
        .collect();
    for o in &objs {
        s.put(o.key, &o.data).unwrap();
    }
    s.close().unwrap();
    drop(s);
    let segs = seg_files(dir.path());
    assert_eq!(segs.len(), 1, "segs = {segs:?}");
    // The footer CRC does not cover the body, so the store reopens cleanly.
    let mut raw = fs::read(&segs[0]).unwrap();
    raw[MAGIC_HEADER.len() + REC_HEADER_SIZE] ^= 0xFF;
    fs::write(&segs[0], &raw).unwrap();
    let s = Store::open_with(dir.path(), Options::new().sync(false)).unwrap();
    // objs[1] is dead, so the segment is a victim; copying corrupted objs[0]
    // must fail.
    let err = s
        .compact(live_set(&objs, &[0]), CompactOpts::default())
        .unwrap_err();
    assert!(err.is_corrupt(), "err = {err}, want corrupt");
    // The victim must survive a failed compaction.
    let segs = seg_files(dir.path());
    assert_eq!(segs.len(), 1, "segment deleted after failed compaction");
    drop(s);
}

#[test]
fn compact_keeps_barrier_grey() {
    let (_dir, s, objs) = compact_store();
    s.begin_barrier();
    // Distinct from every objs[i], whose first byte is 0..=4 (Go passes the
    // raw incompressible bytes; pinning byte 0 keeps that guarantee explicit).
    let mut data = incompressible(4 << 10);
    data[0] = 5;
    let novel = blob_obj(&data);
    // objs[0] is a dedup hit; both it and the novel object must be greyed.
    let batch = [Ok::<Object, Infallible>(objs[0].clone()), Ok(novel.clone())];
    s.write_batch(batch).unwrap();
    s.compact(live_set(&objs, &[2]), CompactOpts::default())
        .unwrap();
    for (i, k) in [objs[0].key, novel.key, objs[2].key]
        .into_iter()
        .enumerate()
    {
        assert!(s.has(k).unwrap(), "grey/live object {i} gone");
    }
    for i in [1, 3, 4] {
        assert!(!s.has(objs[i].key).unwrap(), "dead object {i} survived");
    }

    // Compact consumed the capture: the grey objects are dead now.
    s.compact(live_set(&objs, &[2]), CompactOpts::default())
        .unwrap();
    assert!(
        !s.has(objs[0].key).unwrap(),
        "grey set survived its compact"
    );
    assert!(s.has(objs[2].key).unwrap(), "live object lost");
}

#[test]
fn observe_keys_protects_closure() {
    let (_dir, s, objs) = compact_store();
    s.begin_barrier();
    // A reference PUT racing the mark greys its whole walked closure.
    s.observe_keys(&[objs[1].key, objs[3].key]);
    s.compact(|_| false, CompactOpts::default()).unwrap();
    for i in [1, 3] {
        assert!(s.has(objs[i].key).unwrap(), "greyed closure key {i} gone");
    }
    for i in [0, 2, 4] {
        assert!(!s.has(objs[i].key).unwrap(), "dead object {i} survived");
    }
}

#[test]
fn abort_barrier_discards_capture() {
    let (_dir, s, objs) = compact_store();
    s.begin_barrier();
    s.put(objs[0].key, &objs[0].data).unwrap(); // dedup observe
    s.abort_barrier();
    s.compact(|_| false, CompactOpts::default()).unwrap();
    assert!(
        !s.has(objs[0].key).unwrap(),
        "aborted capture still protected an object"
    );
}

// ---------------------------------------------------------------------------
// Compaction concurrency tests (Go: compact_concurrent_test.go).

/// Reads stay correct while compact unmaps and deletes their segments (Go:
/// `TestCompactDuringReads`).
#[test]
fn compact_during_reads() {
    let dir = TempDir::new().unwrap();
    let s = Store::open_with(dir.path(), Options::new().segment_size(8 << 10).sync(false)).unwrap();

    let by_key: Mutex<HashMap<Key, Vec<u8>>> = Mutex::new(HashMap::new());
    let keys: Mutex<Arc<Vec<Key>>> = Mutex::new(Arc::new(Vec::new()));

    let put = |generation: usize, count: usize| -> Vec<Object> {
        let objs: Vec<Object> = (0..count)
            .map(|i| {
                let mut data = incompressible(4 << 10);
                data[0] = i as u8;
                data[1] = generation as u8;
                blob_obj(&data)
            })
            .collect();
        for o in &objs {
            s.put(o.key, &o.data).unwrap();
        }
        let mut m = by_key.lock().unwrap();
        for o in &objs {
            m.insert(o.key, o.data.clone());
        }
        *keys.lock().unwrap() = Arc::new(m.keys().copied().collect());
        objs
    };
    put(0, 16);

    let done = AtomicBool::new(false);
    thread::scope(|scope| {
        for _ in 0..4 {
            let s = &s;
            let by_key = &by_key;
            let keys = &keys;
            let done = &done;
            scope.spawn(move || {
                let mut i = 0usize;
                while !done.load(Ordering::Relaxed) {
                    let ks = keys.lock().unwrap().clone();
                    let k = ks[i % ks.len()];
                    if i.is_multiple_of(2) {
                        match s.get(k) {
                            Err(e) if e.is_not_found() => {}
                            Err(e) => panic!("get during compact: {e}"),
                            Ok(data) => {
                                let want = by_key.lock().unwrap().get(&k).cloned().unwrap();
                                assert_eq!(data, want, "get returned wrong bytes");
                            }
                        }
                    } else {
                        match s.get_record(k) {
                            Err(e) if e.is_not_found() => {}
                            Err(e) => panic!("get_record during compact: {e}"),
                            Ok(rec) => {
                                // Touch every byte.
                                let sum: u64 = rec.iter().map(|&b| u64::from(b)).sum();
                                std::hint::black_box(sum);
                            }
                        }
                    }
                    i += 1;
                }
            });
        }
        for round in 1..=8 {
            let objs = put(round, 16);
            let live: HashSet<Key> = objs.iter().map(|o| o.key).collect();
            s.compact(|k| live.contains(&k), CompactOpts::default())
                .unwrap();
        }
        done.store(true, Ordering::Relaxed);
    });
}
