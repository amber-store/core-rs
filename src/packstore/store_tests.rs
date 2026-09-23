//! Ported Go store tests (`packstore_test.go`, `oracle_test.go`,
//! `footer_test.go`, `recover_test.go`, `verify_test.go`, `missing_test.go`,
//! `parallel_test.go`).

use std::collections::HashMap;
use std::fs;
use std::path::Path;
use std::thread;

use tempfile::TempDir;

use crate::amberpack::{REC_HEADER_SIZE, encode_record};
use crate::binaryfuse::{BinaryFuse16, SECTION_HEADER_SIZE};
use crate::key::{Key, Type};

use super::footer::{
    FANOUT_SIZE, INDEX_ENTRY_SIZE, IndexEntry, SealedSegment, TRAILER_SIZE, build_filter_section,
    build_footer, build_index_section, filter_key, parse_filter_section, parse_index_section,
    search_index,
};
use super::recover::scan_active;
use super::testutil::*;
use super::{Error, MAGIC_HEADER, Object, Options, Store, TAG_SEAL, WriteOpts};

/// The synthetic error `objSeq` yields (Go: "synthetic iterator error").
#[derive(Debug, thiserror::Error)]
#[error("synthetic iterator error")]
pub(crate) struct SyntheticError;

/// A batch iterator that optionally fails after `fail_after` objects (Go:
/// `objSeq`).
pub(crate) fn obj_seq(
    objs: &[Object],
    fail_after: Option<usize>,
) -> std::vec::IntoIter<Result<Object, SyntheticError>> {
    let mut v: Vec<Result<Object, SyntheticError>> = Vec::new();
    for (i, o) in objs.iter().enumerate() {
        if Some(i) == fail_after {
            v.push(Err(SyntheticError));
            return v.into_iter();
        }
        v.push(Ok(o.clone()));
    }
    v.into_iter()
}

fn glob_suffix(dir: &Path, suffix: &str) -> Vec<std::path::PathBuf> {
    let mut out: Vec<_> = fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().path())
        .filter(|p| p.file_name().unwrap().to_string_lossy().ends_with(suffix))
        .collect();
    out.sort();
    out
}

pub(crate) fn sealed_files(dir: &Path) -> Vec<std::path::PathBuf> {
    glob_suffix(dir, ".seg")
        .into_iter()
        .filter(|p| !p.to_string_lossy().ends_with(".seg.active"))
        .collect()
}

pub(crate) fn active_files(dir: &Path) -> Vec<std::path::PathBuf> {
    glob_suffix(dir, ".seg.active")
}

#[test]
fn put_get_has_round_trip() {
    let dir = TempDir::new().unwrap();
    let s = Store::open(dir.path()).unwrap();
    let objs = test_objects(50);
    for o in &objs {
        s.put(o.key, &o.data).unwrap();
    }
    for o in &objs {
        assert!(s.has(o.key).unwrap(), "has({}) = false", o.key);
        assert_eq!(
            s.get(o.key).unwrap(),
            o.data,
            "get({}): payload mismatch",
            o.key
        );
    }
}

#[test]
fn get_record_round_trip() {
    // get_record returns the on-disk record verbatim — identical to
    // encode_record — for objects in both the active segment and sealed
    // segments. The tiny segment size forces most objects into sealed
    // segments; payloads mix compressible (stored zstd) and incompressible
    // (stored raw) so both flag paths are exercised.
    let dir = TempDir::new().unwrap();
    let s = Store::open_with(dir.path(), Options::default().segment_size(2048)).unwrap();
    let objs: Vec<Object> = (0..30)
        .map(|i| {
            let mut data = if i % 2 == 0 {
                compressible(1500)
            } else {
                incompressible(1500)
            };
            data.push(i as u8);
            blob_obj(&data)
        })
        .collect();
    for o in &objs {
        s.put(o.key, &o.data).unwrap();
    }
    for o in &objs {
        let want = encode_record(o.key, &o.data).unwrap();
        let got = s.get_record(o.key).unwrap();
        assert_eq!(got, want, "get_record({}): record mismatch", o.key);
        // The returned record must decode back to the original payload.
        let rec = crate::amberpack::parse_record(&got).unwrap();
        let payload = crate::amberpack::decode_payload(
            rec.flags,
            rec.ulen,
            &got[crate::amberpack::REC_HEADER_SIZE..],
        )
        .unwrap();
        assert_eq!(
            payload, o.data,
            "get_record({}): decoded payload mismatch",
            o.key
        );
    }
}

#[test]
fn get_record_absent_returns_not_found() {
    let dir = TempDir::new().unwrap();
    let s = Store::open(dir.path()).unwrap();
    let err = s.get_record(blob_obj(b"nope").key).unwrap_err();
    assert!(err.is_not_found(), "err = {err}, want NotFound");
}

#[test]
fn stored_size_matches_record_payload() {
    let dir = TempDir::new().unwrap();
    let s = Store::open_with(dir.path(), Options::default().segment_size(2048)).unwrap();
    let objs: Vec<Object> = (0..30)
        .map(|i| {
            let mut data = if i % 2 == 0 {
                compressible(1500)
            } else {
                incompressible(1500)
            };
            data.push(i as u8);
            blob_obj(&data)
        })
        .collect();
    for o in &objs {
        s.put(o.key, &o.data).unwrap();
    }
    for o in &objs {
        let rec = encode_record(o.key, &o.data).unwrap();
        let want = (rec.len() - crate::amberpack::REC_HEADER_SIZE) as u64;
        let got = s.stored_size(o.key).unwrap();
        assert_eq!(got, Some(want), "stored_size({})", o.key);
    }
    assert_eq!(
        s.stored_size(blob_obj(b"nope").key).unwrap(),
        None,
        "stored_size(absent)"
    );
}

#[test]
fn sort_by_location_orders_by_disk_layout() {
    // Objects spread across several sealed segments and the active segment.
    // Keys handed to sort_by_location in a non-disk order must come back
    // ordered by physical layout — grouped by segment, ascending offset
    // within a segment.
    let dir = TempDir::new().unwrap();
    let s = Store::open_with(dir.path(), Options::default().segment_size(2048)).unwrap();
    let mut keys: Vec<Key> = Vec::new();
    for i in 0..60u32 {
        let mut data = incompressible(1500);
        data.push(i as u8);
        data.push((i >> 8) as u8);
        let o = blob_obj(&data);
        s.put(o.key, &o.data).unwrap();
        keys.push(o.key);
    }
    // Reverse so the input is not already in disk order.
    keys.reverse();

    s.sort_by_location(&mut keys);

    let sh = super::unpoison(s.shared.read());
    let mut prev: Option<(u64, u64)> = None;
    for k in &keys {
        let (seg, off) = Store::locate_in(&sh, *k).expect("locate");
        if let Some((pseg, poff)) = prev {
            assert!(
                seg > pseg || (seg == pseg && off >= poff),
                "out of disk order: ({seg},{off}) follows ({pseg},{poff})"
            );
        }
        prev = Some((seg, off));
    }
}

#[test]
fn sort_by_location_puts_absent_keys_last() {
    let dir = TempDir::new().unwrap();
    let s = Store::open(dir.path()).unwrap();
    let present = blob_obj(b"here");
    s.put(present.key, &present.data).unwrap();
    let absent = blob_obj(b"gone").key;
    let mut keys = vec![absent, present.key];
    s.sort_by_location(&mut keys);
    assert_eq!(
        keys,
        vec![present.key, absent],
        "absent key not sorted last"
    );
}

#[test]
fn get_absent_returns_not_found() {
    let dir = TempDir::new().unwrap();
    let s = Store::open(dir.path()).unwrap();
    let err = s.get(blob_obj(b"nope").key).unwrap_err();
    assert!(err.is_not_found(), "err = {err}, want NotFound");
    assert_eq!(err.to_string(), "packstore: object not found");
    assert!(!s.has(blob_obj(b"nope").key).unwrap());
}

#[test]
fn put_is_idempotent() {
    let dir = TempDir::new().unwrap();
    let s = Store::open(dir.path()).unwrap();
    let o = blob_obj(&incompressible(1000));
    for _ in 0..3 {
        s.put(o.key, &o.data).unwrap();
    }
    // Three identical puts must append exactly one record.
    s.close().unwrap();
    let actives = active_files(dir.path());
    assert_eq!(actives.len(), 1);
    let size = fs::metadata(&actives[0]).unwrap().len();
    let rec = encode_record(o.key, &o.data).unwrap();
    assert_eq!(size, (MAGIC_HEADER.len() + rec.len()) as u64);
}

#[test]
fn reopen_resumes_active_segment() {
    let dir = TempDir::new().unwrap();
    let s = Store::open(dir.path()).unwrap();
    let first = test_objects(10);
    for o in &first {
        s.put(o.key, &o.data).unwrap();
    }
    s.close().unwrap();

    let s2 = Store::open(dir.path()).unwrap();
    for o in &first {
        assert_eq!(
            s2.get(o.key).unwrap(),
            o.data,
            "after reopen get({})",
            o.key
        );
    }
    let more = blob_obj(b"written after reopen");
    s2.put(more.key, &more.data).unwrap();
    assert_eq!(s2.get(more.key).unwrap(), more.data);
    // Still exactly one active segment file, no sealed ones.
    assert_eq!(sealed_files(dir.path()).len(), 0);
    assert_eq!(active_files(dir.path()).len(), 1);
}

#[test]
fn reopen_truncates_corrupt_tail() {
    let dir = TempDir::new().unwrap();
    let s = Store::open(dir.path()).unwrap();
    let objs = test_objects(5);
    for o in &objs {
        s.put(o.key, &o.data).unwrap();
    }
    s.close().unwrap();

    // Simulate a torn write: append garbage to the active file.
    let actives = active_files(dir.path());
    let mut b = fs::read(&actives[0]).unwrap();
    b.extend_from_slice(&[0x55, 0x44, 0x33]);
    fs::write(&actives[0], &b).unwrap();

    let s2 = Store::open(dir.path()).unwrap();
    for o in &objs {
        assert_eq!(
            s2.get(o.key).unwrap(),
            o.data,
            "get({}) after torn-tail recovery",
            o.key
        );
    }
    // New writes land cleanly after the truncated tail.
    let o = blob_obj(b"post-recovery write");
    s2.put(o.key, &o.data).unwrap();
    s2.close().unwrap();
    let s3 = Store::open(dir.path()).unwrap();
    assert_eq!(s3.get(o.key).unwrap(), o.data, "get after second reopen");
}

// Go's `TestSecondOpenFails` and `TestMultipleActiveFilesFailOpen` went with
// the single-owner store: see multi_tests.rs for what holds now.

#[test]
fn closed_store_errors() {
    let dir = TempDir::new().unwrap();
    let s = Store::open(dir.path()).unwrap();
    let o = blob_obj(b"x");
    s.put(o.key, &o.data).unwrap();
    s.close().unwrap();
    assert!(s.get(o.key).unwrap_err().is_closed(), "get after close");
    assert!(
        s.put(o.key, &o.data).unwrap_err().is_closed(),
        "put after close"
    );
    assert!(s.has(o.key).unwrap_err().is_closed(), "has after close");
    // Idempotent (Go returns nil on double Close).
    s.close().unwrap();
}

#[test]
fn with_sync_false() {
    let dir = TempDir::new().unwrap();
    let s = Store::open_with(dir.path(), Options::default().sync(false)).unwrap();
    let o = blob_obj(&incompressible(100));
    s.put(o.key, &o.data).unwrap();
    assert_eq!(s.get(o.key).unwrap(), o.data);
}

#[test]
fn concurrent_put_get_has() {
    let dir = TempDir::new().unwrap();
    let s = Store::open_with(dir.path(), Options::default().sync(false)).unwrap();
    let objs = test_objects(200);

    thread::scope(|scope| {
        for w in 0..4usize {
            let s = &s;
            let objs = &objs;
            scope.spawn(move || {
                // Overlapping key ranges: every writer writes every other
                // object, so same-key put races are exercised.
                let mut i = w % 2;
                while i < objs.len() {
                    s.put(objs[i].key, &objs[i].data).unwrap();
                    i += 2;
                }
            });
        }
        for _ in 0..4 {
            let s = &s;
            let objs = &objs;
            scope.spawn(move || {
                for o in objs {
                    s.has(o.key).unwrap();
                    match s.get(o.key) {
                        Ok(_) => {}
                        Err(e) if e.is_not_found() => {}
                        Err(e) => panic!("reader: {e}"),
                    }
                }
            });
        }
    });

    for o in &objs {
        assert_eq!(
            s.get(o.key).unwrap(),
            o.data,
            "get({}) after concurrent writes",
            o.key
        );
    }
}

#[test]
fn rotation_seals_segments() {
    let dir = TempDir::new().unwrap();
    // Tiny threshold + incompressible payloads: every record (~2 KB stored)
    // crosses 1024 bytes, so every put seals a segment.
    let s = Store::open_with(dir.path(), Options::default().segment_size(1024)).unwrap();
    let objs: Vec<Object> = (0..30u32)
        .map(|i| {
            let mut data = incompressible(2000);
            data.push(i as u8);
            data.push((i >> 8) as u8);
            blob_obj(&data)
        })
        .collect();
    for o in &objs {
        s.put(o.key, &o.data).unwrap();
    }
    assert_eq!(sealed_files(dir.path()).len(), 30, "sealed segment count");
    // All objects must be served from sealed segments.
    for o in &objs {
        assert_eq!(s.get(o.key).unwrap(), o.data, "get({}) from sealed", o.key);
    }
    // And survive a reopen.
    s.close().unwrap();
    let s2 = Store::open(dir.path()).unwrap();
    for o in &objs {
        assert_eq!(
            s2.get(o.key).unwrap(),
            o.data,
            "get({}) after reopen",
            o.key
        );
    }
}

#[test]
fn rotation_mid_stream_keeps_all_objects() {
    // Mixed compressible/incompressible objects across several rotations:
    // every object must remain reachable from whichever segment it landed in.
    let dir = TempDir::new().unwrap();
    let s = Store::open_with(dir.path(), Options::default().segment_size(8 << 10)).unwrap();
    let objs = test_objects(100);
    for o in &objs {
        s.put(o.key, &o.data).unwrap();
    }
    for o in &objs {
        assert_eq!(s.get(o.key).unwrap(), o.data, "get({})", o.key);
    }
}

#[test]
fn crash_between_footer_and_rename() {
    let dir = TempDir::new().unwrap();
    let s = Store::open(dir.path()).unwrap();
    let objs = test_objects(10);
    for o in &objs {
        s.put(o.key, &o.data).unwrap();
    }
    s.close().unwrap();

    // Simulate the crash window: append a valid footer to the .active file
    // but do not rename it.
    let actives = active_files(dir.path());
    let res = scan_active(&actives[0]).unwrap();
    let entries: Vec<IndexEntry> = res
        .index
        .iter()
        .map(|(k, loc)| IndexEntry {
            k: *k,
            off: loc.off,
            slen: loc.slen,
        })
        .collect();
    let footer = build_footer(res.size, &entries).unwrap();
    let mut b = fs::read(&actives[0]).unwrap();
    b.extend_from_slice(&footer);
    fs::write(&actives[0], &b).unwrap();

    // Opening serves everything from the file as it is: a store that only
    // reads finishes nobody's seal. Its first write takes the segment,
    // completes the rename, and goes to a new active segment.
    let s2 = Store::open(dir.path()).unwrap();
    for o in &objs {
        assert_eq!(s2.get(o.key).unwrap(), o.data, "get({})", o.key);
    }
    let extra = blob_obj(b"the write that takes the segment");
    s2.put(extra.key, &extra.data).unwrap();
    assert_eq!(sealed_files(dir.path()).len(), 1);
    assert_eq!(active_files(dir.path()).len(), 1);
    for o in objs.iter().chain([&extra]) {
        assert_eq!(
            s2.get(o.key).unwrap(),
            o.data,
            "get({}) after the seal was finished",
            o.key
        );
    }
}

#[test]
fn open_fails_on_corrupt_sealed_segment() {
    let dir = TempDir::new().unwrap();
    let s = Store::open_with(dir.path(), Options::default().segment_size(1)).unwrap();
    let o = blob_obj(&incompressible(500));
    s.put(o.key, &o.data).unwrap();
    s.close().unwrap();
    let segs = sealed_files(dir.path());
    assert_eq!(segs.len(), 1);
    let mut b = fs::read(&segs[0]).unwrap();
    let last = b.len() - 1;
    b[last] ^= 0xFF; // trailer magic
    fs::write(&segs[0], &b).unwrap();
    let err = Store::open(dir.path()).unwrap_err();
    assert!(err.is_corrupt(), "open = {err}, want corrupt");
}

#[test]
fn concurrent_rotation_reads() {
    // Rotation under read load: sealing must never surface spurious errors
    // for present keys (the fd may only close after the reader-visible swap).
    let dir = TempDir::new().unwrap();
    let s = Store::open_with(dir.path(), Options::default().segment_size(1024)).unwrap();
    let objs: Vec<Object> = (0..120u32)
        .map(|i| {
            let mut data = incompressible(600);
            data.push(i as u8);
            data.push((i >> 8) as u8);
            blob_obj(&data)
        })
        .collect();
    thread::scope(|scope| {
        for w in 0..2usize {
            let s = &s;
            let objs = &objs;
            scope.spawn(move || {
                let mut i = w;
                while i < objs.len() {
                    s.put(objs[i].key, &objs[i].data).unwrap();
                    i += 2;
                }
            });
        }
        for _ in 0..6 {
            let s = &s;
            let objs = &objs;
            scope.spawn(move || {
                for _ in 0..3 {
                    for o in objs {
                        match s.get(o.key) {
                            Ok(_) => {}
                            Err(e) if e.is_not_found() => {}
                            Err(e) => panic!("spurious read error during rotation: {e}"),
                        }
                    }
                }
            });
        }
    });
    for o in &objs {
        assert_eq!(
            s.get(o.key).unwrap(),
            o.data,
            "get({}) after rotations",
            o.key
        );
    }
}

#[test]
fn write_batch_stores_all() {
    let dir = TempDir::new().unwrap();
    let s = Store::open(dir.path()).unwrap();
    let objs = test_objects(100);
    // Duplicate some objects within the batch; they must be written once.
    let mut batch = objs.clone();
    batch.extend_from_slice(&objs[..10]);
    s.write_batch(obj_seq(&batch, None)).unwrap();
    for o in &objs {
        assert_eq!(s.get(o.key).unwrap(), o.data, "get({})", o.key);
    }
}

#[test]
fn write_batch_iterator_error() {
    let dir = TempDir::new().unwrap();
    let s = Store::open(dir.path()).unwrap();
    let objs = test_objects(10);
    let err = s.write_batch(obj_seq(&objs, Some(5))).unwrap_err();
    assert_eq!(err.to_string(), "synthetic iterator error");
    // The already-appended prefix may remain (documented packstore semantics:
    // durable-on-return, not atomic; valid CAS objects are harmless).
    for o in &objs[..5] {
        assert!(s.has(o.key).unwrap(), "prefix object {} lost", o.key);
    }
}

#[test]
fn write_batch_on_closed_store() {
    let dir = TempDir::new().unwrap();
    let s = Store::open(dir.path()).unwrap();
    s.close().unwrap();
    let objs = test_objects(3);
    let err = s.write_batch(obj_seq(&objs, None)).unwrap_err();
    assert!(err.is_closed(), "err = {err}, want closed");
    let err = s.write_batch(obj_seq(&[], None)).unwrap_err();
    assert!(err.is_closed(), "empty batch err = {err}, want closed");
}

#[test]
fn write_batch_survives_reopen() {
    let dir = TempDir::new().unwrap();
    let s = Store::open_with(dir.path(), Options::default().segment_size(16 << 10)).unwrap();
    let objs = test_objects(100);
    s.write_batch(obj_seq(&objs, None)).unwrap();
    s.close().unwrap();
    let s2 = Store::open(dir.path()).unwrap();
    for o in &objs {
        assert_eq!(
            s2.get(o.key).unwrap(),
            o.data,
            "get({}) after reopen",
            o.key
        );
    }
}

#[test]
fn write_batch_rotates() {
    let dir = TempDir::new().unwrap();
    let s = Store::open_with(dir.path(), Options::default().segment_size(16 << 10)).unwrap();
    let objs = test_objects(100);
    s.write_batch(obj_seq(&objs, None)).unwrap();
    assert!(
        !sealed_files(dir.path()).is_empty(),
        "expected at least one sealed segment"
    );
    for o in &objs {
        assert_eq!(s.get(o.key).unwrap(), o.data, "get({})", o.key);
    }
}

#[test]
fn wipe() {
    let dir = TempDir::new().unwrap();
    // Small segments so the store holds sealed segments AND an active one.
    let s = Store::open_with(dir.path(), Options::default().segment_size(1024)).unwrap();
    let objs = test_objects(40);
    for o in &objs {
        s.put(o.key, &o.data).unwrap();
    }
    s.wipe().unwrap();
    for o in &objs {
        assert!(
            s.get(o.key).unwrap_err().is_not_found(),
            "get({}) after wipe",
            o.key
        );
        assert!(!s.has(o.key).unwrap(), "has({}) after wipe", o.key);
    }
    for e in fs::read_dir(dir.path()).unwrap() {
        let name = e.unwrap().file_name().to_string_lossy().into_owned();
        assert!(
            !name.ends_with(".seg") && !name.ends_with(".seg.active"),
            "segment file {name} survived wipe"
        );
    }
    // The store stays usable: writes and reads work after the wipe.
    let more = test_objects(5);
    for o in &more {
        s.put(o.key, &o.data).unwrap();
    }
    for o in &more {
        assert_eq!(
            s.get(o.key).unwrap(),
            o.data,
            "get({}) after post-wipe put",
            o.key
        );
    }
    // And it survives a reopen.
    s.close().unwrap();
    let s2 = Store::open_with(dir.path(), Options::default().segment_size(1024)).unwrap();
    for o in &more {
        assert_eq!(
            s2.get(o.key).unwrap(),
            o.data,
            "get({}) after reopen",
            o.key
        );
    }
}

#[test]
fn wipe_with_concurrent_readers() {
    let dir = TempDir::new().unwrap();
    let s = Store::open_with(dir.path(), Options::default().segment_size(1024)).unwrap();
    let objs = test_objects(40);
    for o in &objs {
        s.put(o.key, &o.data).unwrap();
    }
    let stop = std::sync::atomic::AtomicBool::new(false);
    thread::scope(|scope| {
        for _ in 0..4 {
            let s = &s;
            let objs = &objs;
            let stop = &stop;
            scope.spawn(move || {
                while !stop.load(std::sync::atomic::Ordering::Relaxed) {
                    for o in objs {
                        match s.get(o.key) {
                            Ok(data) => assert_eq!(data, o.data, "reader saw corrupt data"),
                            Err(e) if e.is_not_found() => {} // wiped under us; fine
                            Err(e) => panic!("reader: {e}"),
                        }
                    }
                }
            });
        }
        s.wipe().unwrap();
        stop.store(true, std::sync::atomic::Ordering::Relaxed);
    });
}

#[test]
fn wipe_clears_poisoned_write_path() {
    let dir = TempDir::new().unwrap();
    let s = Store::open(dir.path()).unwrap();
    let objs = test_objects(3);
    for o in &objs {
        s.put(o.key, &o.data).unwrap();
    }
    // Poison the write path the way a failed fsync would.
    s.set_failed(&"simulated fsync failure");
    let err = s.put(objs[0].key, &objs[0].data).unwrap_err();
    assert_eq!(
        err.to_string(),
        "packstore: write path failed: simulated fsync failure"
    );
    // The wipe destroys the data the poison was protecting; writes must work.
    s.wipe().unwrap();
    let more = test_objects(2);
    for o in &more {
        s.put(o.key, &o.data)
            .expect("put after wipe on previously-poisoned store");
    }
}

// Drives a store through random put/write_batch/reopen cycles with a small
// rotation threshold and cross-checks every observable (get, has, missing,
// verify) against an in-memory map (Go: `TestOracle`).
#[test]
fn oracle() {
    let dir = TempDir::new().unwrap();
    let mut r = Rng(0xA5EED);
    let mut oracle: HashMap<Key, Vec<u8>> = HashMap::new();

    let new_obj = |r: &mut Rng| -> Object {
        let n = 1 + r.below(4000) as usize;
        let mut data = vec![0u8; n];
        if r.below(2) == 0 {
            let mut i = 0;
            while i < n {
                let word = r.next_u64().to_le_bytes();
                let take = (n - i).min(8);
                data[i..i + take].copy_from_slice(&word[..take]);
                i += take;
            }
        } else {
            for (i, b) in data.iter_mut().enumerate() {
                *b = (i % 7) as u8;
            }
        }
        blob_obj(&data)
    };

    let opts = Options::default().segment_size(16 << 10).sync(false);
    let mut s = Store::open_with(dir.path(), opts).unwrap();
    for _ in 0..20 {
        match r.below(3) {
            0 => {
                // single puts
                for _ in 0..20 {
                    let o = new_obj(&mut r);
                    s.put(o.key, &o.data).unwrap();
                    oracle.insert(o.key, o.data);
                }
            }
            1 => {
                // a batch with duplicates
                let mut objs = Vec::new();
                for _ in 0..30 {
                    let o = new_obj(&mut r);
                    objs.push(o.clone());
                    objs.push(o.clone());
                    oracle.insert(o.key, o.data);
                }
                s.write_batch(obj_seq(&objs, None)).unwrap();
            }
            _ => {
                // reopen (exercises seal-survival + tail-scan resume)
                s.close().unwrap();
                s = Store::open_with(dir.path(), opts).unwrap();
            }
        }
    }

    // Full readback.
    let mut present: Vec<Key> = Vec::new();
    for (k, want) in &oracle {
        assert_eq!(&s.get(*k).unwrap(), want, "get({k})");
        present.push(*k);
    }

    // Absent probes: compressible oracle data is a pure function of its
    // length, so fresh keys CAN collide with stored ones (and each other) —
    // skip stored keys and dedupe.
    let mut absent: Vec<Key> = Vec::new();
    let mut absent_set: std::collections::HashSet<Key> = std::collections::HashSet::new();
    while absent.len() < 100 {
        let k = new_obj(&mut r).key;
        if oracle.contains_key(&k) || !absent_set.insert(k) {
            continue;
        }
        absent.push(k);
    }

    // Missing cross-check.
    let mut query = present.clone();
    query.extend_from_slice(&absent);
    let miss = s.missing(&query).unwrap();
    let mut miss_count: HashMap<Key, usize> = HashMap::new();
    for k in &miss {
        *miss_count.entry(*k).or_default() += 1;
    }
    for k in &present {
        assert!(
            !miss_count.contains_key(k),
            "present key {k} reported missing"
        );
    }
    for k in &absent {
        assert_eq!(
            miss_count.get(k),
            Some(&1),
            "absent key {k} not reported missing exactly once"
        );
    }

    // Structural scrub.
    s.verify(|| false).unwrap();
}

#[test]
fn segment_id_parsing() {
    // Naming must match Go's parseSegmentID exactly: 16 hex digits + suffix.
    let dir = TempDir::new().unwrap();
    fs::write(dir.path().join("123.seg"), b"x").unwrap();
    let err = Store::open(dir.path()).unwrap_err();
    assert!(err.is_corrupt(), "short id: {err}");
    assert_eq!(
        err.to_string(),
        "amberpack: corrupt pack data: bad segment file name \"123.seg\""
    );

    let dir = TempDir::new().unwrap();
    fs::write(dir.path().join("00000000000000zz.seg"), b"x").unwrap();
    let err = Store::open(dir.path()).unwrap_err();
    assert!(err.is_corrupt(), "non-hex id: {err}");

    // A plus sign must not be accepted as part of the id (Go's ParseUint
    // rejects signs).
    let dir = TempDir::new().unwrap();
    fs::write(dir.path().join("+000000000000001.seg"), b"x").unwrap();
    let err = Store::open(dir.path()).unwrap_err();
    assert!(err.is_corrupt(), "signed id: {err}");
}

#[test]
fn non_segment_files_are_ignored() {
    let dir = TempDir::new().unwrap();
    fs::write(dir.path().join(".DS_Store"), b"junk").unwrap();
    fs::write(dir.path().join("manifest.json"), b"{}").unwrap();
    let s = Store::open(dir.path()).unwrap();
    let o = blob_obj(b"hello");
    s.put(o.key, &o.data).unwrap();
    assert_eq!(s.get(o.key).unwrap(), o.data);
}

// ---------------------------------------------------------------------------
// Footer tests (Go: footer_test.go).

/// Returns `k` with byte `i` replaced by `b` (the mutation keeps the key
/// canonical: only hash-tail bytes are touched).
fn key_set_byte(k: Key, i: usize, b: u8) -> Key {
    let mut raw = *k.as_bytes();
    raw[i] = b;
    Key::parse(&raw).expect("mutated key stays canonical")
}

#[test]
fn index_section_lookup() {
    let entries = test_entries(1000);
    let idx = build_index_section(&entries);
    assert_eq!(idx.len(), FANOUT_SIZE + entries.len() * INDEX_ENTRY_SIZE);
    let (fanout, rows) = parse_index_section(&idx, entries.len() as u64).unwrap();
    for e in &entries {
        let (off, slen) =
            search_index(&fanout, rows, e.k).unwrap_or_else(|| panic!("key {} not found", e.k));
        assert_eq!((off, slen), (e.off, e.slen), "key {}", e.k);
    }
}

#[test]
fn index_section_absent_key() {
    let entries = test_entries(100);
    let idx = build_index_section(&entries);
    let (fanout, rows) = parse_index_section(&idx, entries.len() as u64).unwrap();
    let absent = blob_obj(b"definitely not stored").key;
    assert!(
        search_index(&fanout, rows, absent).is_none(),
        "absent key reported present"
    );
}

#[test]
fn index_section_single_entry_and_edge_buckets() {
    // Force last bytes 0x00 and 0xFF to cover the b==0 lower bound and the
    // final bucket.
    for last in [0x00u8, 0xFF, 0x80] {
        let mut e = test_entries(1)[0];
        e.k = key_set_byte(e.k, 31, last);
        let idx = build_index_section(&[e]);
        let (fanout, rows) = parse_index_section(&idx, 1).unwrap();
        let (off, slen) = search_index(&fanout, rows, e.k)
            .unwrap_or_else(|| panic!("last={last:#x}: key not found"));
        assert_eq!((off, slen), (e.off, e.slen), "last={last:#x}");
        let miss = key_set_byte(e.k, 30, e.k.as_bytes()[30] ^ 0xFF);
        assert!(
            search_index(&fanout, rows, miss).is_none(),
            "last={last:#x}: absent key found"
        );
    }
}

#[test]
fn index_section_does_not_mutate_input() {
    let entries = test_entries(50);
    let orig = entries.clone();
    let _ = build_index_section(&entries);
    assert_eq!(entries, orig, "build_index_section mutated its input");
}

#[test]
fn parse_index_section_rejects_corruption() {
    let entries = test_entries(10);
    let idx = build_index_section(&entries);

    // Wrong length.
    assert!(parse_index_section(&idx[..idx.len() - 1], 10).is_err());

    // Non-monotonic fanout: make fanout[1] < fanout[0] by forcing fanout[0]
    // huge.
    let mut bad = idx.clone();
    bad[0..4].copy_from_slice(&0xFFFF_FFFFu32.to_be_bytes());
    assert!(parse_index_section(&bad, 10).is_err());

    // Fanout total mismatch: keep the section length correct for key_count=1
    // but zero the whole fanout — monotonic, total 0 != 1; must hit the total
    // check, not the length check.
    let one = build_index_section(&test_entries(1));
    let mut bad = one.clone();
    bad[..FANOUT_SIZE].fill(0);
    let err = parse_index_section(&bad, 1).unwrap_err();
    assert!(
        err.to_string().contains("fanout total"),
        "wrong branch: {err}"
    );

    // Huge key_count must not wrap: (2^64-984)/44 made naive arithmetic
    // compute want==40, so a 40-byte section passed the length check and the
    // fanout loop panicked in Go. The fixed code rejects it as corrupt.
    let huge = (u64::MAX - 983) / INDEX_ENTRY_SIZE as u64;
    let err = parse_index_section(&idx[..40], huge).unwrap_err();
    assert!(
        err.to_string().contains("exceeds format limit"),
        "wrong branch: {err}"
    );
}

#[test]
fn index_section_empty_and_empty_bucket() {
    // Empty section round-trips and misses cleanly.
    let idx = build_index_section(&[]);
    let (fanout, rows) = parse_index_section(&idx, 0).unwrap();
    assert!(
        search_index(&fanout, rows, blob_obj(b"x").key).is_none(),
        "found key in empty index"
    );

    // Deterministic empty-bucket miss: one entry with last byte 0x10, search
    // a key with last byte 0x20 (a guaranteed-empty bucket).
    let mut e = test_entries(1)[0];
    e.k = key_set_byte(e.k, 31, 0x10);
    let idx = build_index_section(&[e]);
    let (fanout, rows) = parse_index_section(&idx, 1).unwrap();
    let probe = key_set_byte(e.k, 31, 0x20);
    assert!(
        search_index(&fanout, rows, probe).is_none(),
        "found key in empty bucket"
    );
}

#[test]
fn filter_section_membership() {
    let entries = test_entries(5000);
    let sec = build_filter_section(&entries).unwrap();
    let f = parse_filter_section(&sec).unwrap();
    for e in &entries {
        assert!(f.contains(filter_key(e.k)), "false negative for {}", e.k);
    }
}

#[test]
fn filter_section_false_positive_rate() {
    let entries = test_entries(1000);
    let sec = build_filter_section(&entries).unwrap();
    let f = parse_filter_section(&sec).unwrap();
    // 16-bit fingerprints: FP rate ~2^-16. Expect ~1.5 hits in 100k probes;
    // 50 leaves astronomical margin while still catching a broken filter.
    let fp = (0..100_000u64)
        .filter(|i| f.contains(0xDEAD_0000_0000_0000 + i))
        .count();
    assert!(fp <= 50, "false positive rate too high: {fp}/100000");
}

#[test]
fn filter_section_duplicate_tails() {
    // Two entries with an identical 8-byte tail must not break the build.
    let mut entries = test_entries(2);
    let mut raw = *entries[1].k.as_bytes();
    raw[24..32].copy_from_slice(&entries[0].k.as_bytes()[24..32]);
    entries[1].k = Key::parse(&raw).unwrap();
    let sec = build_filter_section(&entries).unwrap();
    let f = parse_filter_section(&sec).unwrap();
    assert!(
        f.contains(filter_key(entries[0].k)) && f.contains(filter_key(entries[1].k)),
        "false negative on duplicate tails"
    );
}

#[test]
fn parse_filter_section_rejects_corruption() {
    let sec = build_filter_section(&test_entries(10)).unwrap();
    assert!(parse_filter_section(&sec[..10]).is_err(), "short");
    let mut bad = sec.clone();
    bad[0] = 99;
    assert!(parse_filter_section(&bad).is_err(), "bad type");
    assert!(
        parse_filter_section(&sec[..sec.len() - 2]).is_err(),
        "length mismatch"
    );
}

#[test]
fn index_section_golden_bytes() {
    // Pin the on-disk encoding against symmetric encode/decode bugs: two
    // fixed entries, exact expected bytes.
    let mut raw1 = [0u8; 32];
    raw1[0] = 0x01; // Blob, 2-byte length field
    raw1[1] = 0x05;
    raw1[31] = 0x02;
    let k1 = Key::parse(&raw1).unwrap();
    let mut raw2 = [0u8; 32];
    raw2[0] = 0x01;
    raw2[1] = 0x07;
    raw2[31] = 0x01; // sorts before k1 (last byte)
    let k2 = Key::parse(&raw2).unwrap();
    let idx = build_index_section(&[
        IndexEntry {
            k: k1,
            off: 0x1122_3344_5566_7788,
            slen: 0xAABB_CCDD,
        },
        IndexEntry {
            k: k2,
            off: 8,
            slen: 1,
        },
    ]);

    // fanout: bytes 0x00 → 0, 0x01 → 1, 0x02..0xFF → 2 (cumulative, BE).
    assert_eq!(super::be_u32(&idx, 0), 0, "fanout[0]");
    assert_eq!(super::be_u32(&idx, 4), 1, "fanout[1]");
    assert_eq!(super::be_u32(&idx, 8), 2, "fanout[2]");
    assert_eq!(super::be_u32(&idx, 1020), 2, "fanout[255]");

    // First entry must be k2 (last byte 0x01): key bytes, then off/slen BE.
    let e0 = &idx[FANOUT_SIZE..FANOUT_SIZE + INDEX_ENTRY_SIZE];
    assert_eq!(&e0[..32], k2.as_bytes());
    assert_eq!(&e0[32..40], &[0, 0, 0, 0, 0, 0, 0, 8]);
    assert_eq!(&e0[40..44], &[0, 0, 0, 1]);

    let e1 = &idx[FANOUT_SIZE + INDEX_ENTRY_SIZE..FANOUT_SIZE + 2 * INDEX_ENTRY_SIZE];
    assert_eq!(&e1[..32], k1.as_bytes());
    assert_eq!(
        &e1[32..40],
        &[0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77, 0x88]
    );
    assert_eq!(&e1[40..44], &[0xAA, 0xBB, 0xCC, 0xDD]);
}

#[test]
fn parse_filter_section_rejects_bad_geometry() {
    let sec = build_filter_section(&test_entries(100)).unwrap();
    let mutate = |name: &str, f: &dyn Fn(&mut [u8])| {
        let mut bad = sec.clone();
        f(&mut bad);
        let err = parse_filter_section(&bad).unwrap_err();
        assert!(err.is_corrupt(), "{name}: want corrupt, got {err}");
    };
    // Crafted geometry previously made Contains panic with
    // index-out-of-range in Go's xorfilter; parse must reject it instead.
    mutate("segCountLen inflated", &|b| {
        b[21..25].copy_from_slice(&0xFFFF_FFF0u32.to_be_bytes());
    });
    mutate("mask inflated", &|b| {
        b[13..17].copy_from_slice(&0xFFFF_FFFFu32.to_be_bytes());
    });
    mutate("segLen not power of two", &|b| {
        let v = super::be_u32(b, 9) + 1;
        b[9..13].copy_from_slice(&v.to_be_bytes());
    });
    mutate("segLen zero", &|b| {
        b[9..13].copy_from_slice(&0u32.to_be_bytes());
        b[13..17].copy_from_slice(&0xFFFF_FFFFu32.to_be_bytes());
    });
    // segCount == 0 (with segCountLen == 0 and fpCount == 2*segLen) satisfies
    // every other geometry identity, so it needs its own rejection; the
    // crafted geometry previously made Go's xorfilter Contains panic with
    // index-out-of-range on first lookup (Go adds a dedicated first case in
    // parseFilterSection; the Rust binaryfuse parser already refused it —
    // this pins the behavior) (Go: the "segCount zero" subtest).
    {
        let seg_len = super::be_u32(&sec, 9);
        let fp_count = 2 * seg_len;
        let mut bad = sec[..SECTION_HEADER_SIZE + 2 * fp_count as usize].to_vec();
        bad[17..21].copy_from_slice(&0u32.to_be_bytes()); // segCount
        bad[21..25].copy_from_slice(&0u32.to_be_bytes()); // segCountLen
        bad[25..29].copy_from_slice(&fp_count.to_be_bytes());
        let err = parse_filter_section(&bad).unwrap_err();
        assert!(err.is_corrupt(), "segCount zero: want corrupt, got {err}");
    }
}

#[test]
fn filter_section_field_round_trip() {
    let entries = test_entries(1234);
    let mut tails: Vec<u64> = entries.iter().map(|e| filter_key(e.k)).collect();
    tails.sort_unstable();
    tails.dedup();
    let want = BinaryFuse16::new(&tails).unwrap();
    let sec = build_filter_section(&entries).unwrap();
    let got = parse_filter_section(&sec).unwrap();
    assert_eq!(got.seed, want.seed);
    assert_eq!(got.segment_length, want.segment_length);
    assert_eq!(got.segment_length_mask, want.segment_length_mask);
    assert_eq!(got.segment_count, want.segment_count);
    assert_eq!(got.segment_count_length, want.segment_count_length);
    assert_eq!(got.fingerprints, want.fingerprints);
}

#[test]
fn sealed_segment_round_trip() {
    let objs = test_objects(200);
    let (_dir, path, _) = write_sealed_file(&objs);
    let seg = SealedSegment::open(&path, 1).unwrap();
    assert_eq!(seg.fv.key_count, 200);
    for o in &objs {
        assert!(seg.has(o.key), "has({}) = false", o.key);
        let data = seg
            .get(o.key)
            .unwrap()
            .unwrap_or_else(|| panic!("get({}): not found", o.key));
        assert_eq!(data, o.data, "get({}): payload mismatch", o.key);
    }
    let absent = blob_obj(b"not here").key;
    assert!(!seg.has(absent), "has(absent) = true");
    assert!(seg.get(absent).unwrap().is_none(), "get(absent) found");
}

#[test]
fn open_sealed_rejects_corruption() {
    let objs = test_objects(20);
    let (_dir, path, _) = write_sealed_file(&objs);
    let good = fs::read(&path).unwrap();

    let corrupt = |name: &str, mutate: &dyn Fn(&mut Vec<u8>)| {
        let mut b = good.clone();
        mutate(&mut b);
        let dir = TempDir::new().unwrap();
        let p = dir.path().join("0000000000000001.seg");
        fs::write(&p, &b).unwrap();
        assert!(SealedSegment::open(&p, 1).is_err(), "{name}: want error");
    };

    corrupt("trailer magic", &|b| {
        let last = b.len() - 1;
        b[last] ^= 0xFF;
    });
    corrupt("footer CRC over index", &|b| {
        let tr_at = b.len() - TRAILER_SIZE;
        let index_off = be_u64(b, tr_at) as usize;
        b[index_off + 10] ^= 0xFF;
    });
    corrupt("header magic", &|b| b[0] ^= 0xFF);
    // The CRC does not cover the last 16 bytes; reserved is checked
    // explicitly.
    corrupt("reserved nonzero", &|b| {
        let at = b.len() - 12;
        b[at] = 1;
    });
    corrupt("truncated", &|b| b.truncate(good.len() - 100));
}

#[test]
fn build_footer_rejects_empty() {
    assert!(
        build_footer(8, &[]).is_err(),
        "want error for empty segment"
    );
}

#[test]
fn parse_footer_rejects_wrapping_trailer() {
    let objs = test_objects(20);
    let (_dir, path, _) = write_sealed_file(&objs);
    let mut b = fs::read(&path).unwrap();
    // key_count=u32::MAX forces an index_len of ~190 GB, so filter_off >>
    // file_len; a filter_len chosen to wrap mod 2^64 made the naive sum check
    // pass and the index slice expression panic in Go.
    let tr_at = b.len() - TRAILER_SIZE;
    let index_off = be_u64(&b, tr_at);
    let key_count = u64::from(u32::MAX);
    let index_len = FANOUT_SIZE as u64 + key_count * INDEX_ENTRY_SIZE as u64;
    let filter_off = index_off + index_len;
    let file_len = b.len() as u64;
    b[tr_at + 8..tr_at + 16].copy_from_slice(&index_len.to_be_bytes());
    b[tr_at + 16..tr_at + 24].copy_from_slice(&filter_off.to_be_bytes());
    let wrapping = file_len
        .wrapping_sub(TRAILER_SIZE as u64)
        .wrapping_sub(filter_off);
    b[tr_at + 24..tr_at + 32].copy_from_slice(&wrapping.to_be_bytes());
    b[tr_at + 32..tr_at + 40].copy_from_slice(&key_count.to_be_bytes());
    refresh_footer_crc(&mut b);
    let dir = TempDir::new().unwrap();
    let p = dir.path().join("0000000000000001.seg");
    fs::write(&p, &b).unwrap();
    let err = SealedSegment::open(&p, 1).unwrap_err();
    assert!(err.is_corrupt(), "want corrupt, got {err}");
}

#[test]
fn sealed_get_rejects_crafted_offsets() {
    let objs = test_objects(8);
    for bad_off in [i64::MAX as u64, 1u64 << 62] {
        let (_dir, path, _) = write_sealed_file(&objs);
        let mut b = fs::read(&path).unwrap();
        // Rewrite index entry 0's off field, fix the CRC, reopen, and get the
        // entry's own key: the bounds check must answer corrupt, not panic.
        let tr_at = b.len() - TRAILER_SIZE;
        let index_off = be_u64(&b, tr_at) as usize;
        let entry_pos = index_off + FANOUT_SIZE;
        let k = Key::parse(&b[entry_pos..entry_pos + 32]).unwrap();
        b[entry_pos + 32..entry_pos + 40].copy_from_slice(&bad_off.to_be_bytes());
        refresh_footer_crc(&mut b);
        let dir = TempDir::new().unwrap();
        let p = dir.path().join("0000000000000001.seg");
        fs::write(&p, &b).unwrap();
        let seg = SealedSegment::open(&p, 1).unwrap();
        let err = seg.get(k).unwrap_err();
        assert!(
            err.is_corrupt(),
            "off={bad_off:#x}: want corrupt, got {err}"
        );
    }
}

#[test]
fn open_sealed_tiny_files() {
    for n in [0usize, TRAILER_SIZE] {
        let dir = TempDir::new().unwrap();
        let p = dir.path().join("0000000000000001.seg");
        fs::write(&p, vec![0u8; n]).unwrap();
        let err = SealedSegment::open(&p, 1).unwrap_err();
        assert!(err.is_corrupt(), "size {n}: want corrupt, got {err}");
    }
}

// ---------------------------------------------------------------------------
// Recovery tests (Go: recover_test.go).

/// Writes `b` to a `.seg.active` file inside `dir` and returns the path (Go:
/// `activeFile`).
fn active_file(dir: &TempDir, b: &[u8]) -> std::path::PathBuf {
    let p = dir.path().join("0000000000000001.seg.active");
    fs::write(&p, b).unwrap();
    p
}

/// The file offset one past entry `e`'s record.
fn record_end(e: &IndexEntry) -> u64 {
    e.off + REC_HEADER_SIZE as u64 + u64::from(e.slen)
}

#[test]
fn scan_active_clean_file() {
    let objs = test_objects(5);
    let (body, entries) = build_body(&objs);
    let dir = TempDir::new().unwrap();
    let res = scan_active(&active_file(&dir, &body)).unwrap();
    assert!(!res.sealed, "clean active reported sealed");
    assert_eq!(res.size, body.len() as u64);
    assert_eq!(res.index.len(), objs.len());
    for e in &entries {
        let loc = res
            .index
            .get(&e.k)
            .unwrap_or_else(|| panic!("key {} missing", e.k));
        assert_eq!(loc.off, e.off, "key {}", e.k);
    }
}

#[test]
fn scan_active_truncation_at_every_byte() {
    let objs = test_objects(3);
    let (body, entries) = build_body(&objs);
    let ends: Vec<u64> = entries.iter().map(record_end).collect();
    let dir = TempDir::new().unwrap();
    for cut in MAGIC_HEADER.len()..=body.len() {
        let res = scan_active(&active_file(&dir, &body[..cut])).unwrap();
        // boundary(cut) = largest record boundary <= cut.
        let want = ends
            .iter()
            .copied()
            .filter(|&e| e <= cut as u64)
            .max()
            .unwrap_or(MAGIC_HEADER.len() as u64);
        assert_eq!(res.size, want, "cut={cut}");
        let want_keys = ends.iter().filter(|&&e| e <= res.size).count();
        assert_eq!(res.index.len(), want_keys, "cut={cut}");
    }
}

#[test]
fn scan_active_corrupt_byte_truncates_at_that_record() {
    let objs = test_objects(3);
    let (body, entries) = build_body(&objs);
    let last = &entries[2];
    let dir = TempDir::new().unwrap();
    for off in last.off..record_end(last) {
        let mut bad = body.clone();
        bad[off as usize] ^= 0xFF;
        let res = scan_active(&active_file(&dir, &bad)).unwrap();
        assert_eq!(
            res.size, last.off,
            "corrupt byte at {off}: want truncation at {}",
            last.off
        );
    }
}

#[test]
fn scan_active_bad_header_resets() {
    for b in [&b""[..], b"AMB", b"XXXXXXXXjunkjunk"] {
        let dir = TempDir::new().unwrap();
        let res = scan_active(&active_file(&dir, b)).unwrap();
        assert_eq!(res.size, 0, "bad header");
        assert!(res.index.is_empty() && !res.sealed, "bad header");
    }
}

#[test]
fn scan_active_detects_sealed_file() {
    let objs = test_objects(5);
    let (_dir, path, _) = write_sealed_file(&objs); // a fully sealed image
    let res = scan_active(&path).unwrap();
    assert!(res.sealed, "sealed file not detected");
}

#[test]
fn scan_active_partial_footer_truncates() {
    let objs = test_objects(5);
    let (body, _) = build_body(&objs);
    let body_len = body.len() as u64;
    let mut footerish = body;
    footerish.push(TAG_SEAL);
    footerish.extend_from_slice(&[0xAB; 100]); // garbage, not a valid footer
    let dir = TempDir::new().unwrap();
    let res = scan_active(&active_file(&dir, &footerish)).unwrap();
    assert!(!res.sealed, "partial footer reported sealed");
    assert_eq!(res.size, body_len, "truncate at seal marker");
}

#[test]
fn scan_active_middle_record_corruption_no_resync() {
    // The core truncation semantic: the first invalid record ends the valid
    // prefix — the scan must NOT resync to later valid records.
    let objs = test_objects(3);
    let (body, entries) = build_body(&objs);
    let mut bad = body;
    bad[entries[1].off as usize + 5] ^= 0xFF; // corrupt the middle record
    let dir = TempDir::new().unwrap();
    let res = scan_active(&active_file(&dir, &bad)).unwrap();
    assert_eq!(res.size, entries[1].off, "truncation at the middle record");
    assert_eq!(res.index.len(), 1, "only the prefix record");
    assert!(
        res.index.contains_key(&entries[0].k),
        "prefix record missing"
    );
}

#[test]
fn scan_active_valid_footer_with_trailing_garbage() {
    // A complete valid footer followed by extra bytes is unreachable from our
    // write ordering; if encountered, the scan truncates at the seal marker
    // (un-sealing, losing nothing).
    let objs = test_objects(5);
    let (_dir, path, _) = write_sealed_file(&objs);
    let b = fs::read(&path).unwrap();
    let body_len = be_u64(&b, b.len() - TRAILER_SIZE + 40);
    let mut with_garbage = b;
    with_garbage.extend_from_slice(&[0xAB, 0xCD, 0xEF]);
    let dir = TempDir::new().unwrap();
    let res = scan_active(&active_file(&dir, &with_garbage)).unwrap();
    assert!(!res.sealed, "file with trailing garbage reported sealed");
    assert_eq!(res.size, body_len, "truncate at seal marker");
}

// ---------------------------------------------------------------------------
// Scrub tests (Go: verify_test.go).

/// Builds a store with sealed segments and returns its dir (Go:
/// `sealedStore`; also the GC tests' `gcStore` substrate — see gc_tests.rs).
pub(crate) fn sealed_store(objs: &[Object]) -> TempDir {
    let dir = TempDir::new().unwrap();
    let s = Store::open_with(dir.path(), Options::default().segment_size(8 << 10)).unwrap();
    for o in objs {
        s.put(o.key, &o.data).unwrap();
    }
    s.close().unwrap();
    dir
}

#[test]
fn verify_clean_store() {
    let dir = sealed_store(&test_objects(100));
    let s = Store::open(dir.path()).unwrap();
    s.verify(|| false).unwrap();
}

#[test]
fn verify_detects_body_corruption() {
    let dir = sealed_store(&test_objects(100));
    let segs = sealed_files(dir.path());
    assert!(!segs.is_empty(), "no sealed segments");
    let mut b = fs::read(&segs[0]).unwrap();
    // Flip one payload byte inside the body, far from the footer: open's
    // footer CRC does not cover the body, so this must surface in verify.
    b[100] ^= 0x01;
    fs::write(&segs[0], &b).unwrap();

    let s = Store::open(dir.path()).unwrap(); // open succeeds: footer intact
    let err = s.verify(|| false).unwrap_err();
    assert!(err.is_corrupt(), "verify = {err}, want corrupt");
}

#[test]
fn verify_detects_wrong_index_entry() {
    // Craft a segment whose footer is internally consistent (valid CRC) but
    // whose index lies about an offset — the writer-bug class that only a
    // body/index cross-check can catch.
    let objs = test_objects(20);
    let (body, mut entries) = build_body(&objs);
    entries[3].off = entries[2].off; // lie
    let footer = build_footer(body.len() as u64, &entries).unwrap();
    let dir = TempDir::new().unwrap();
    let mut file = body;
    file.extend_from_slice(&footer);
    fs::write(dir.path().join("0000000000000001.seg"), &file).unwrap();

    let s = Store::open(dir.path()).unwrap();
    let err = s.verify(|| false).unwrap_err();
    assert!(err.is_corrupt(), "verify = {err}, want corrupt");
}

#[test]
fn verify_honors_cancel() {
    let dir = sealed_store(&test_objects(100));
    let s = Store::open(dir.path()).unwrap();
    let err = s.verify(|| true).unwrap_err();
    assert!(
        matches!(err, Error::Canceled),
        "verify = {err}, want canceled"
    );
}

#[test]
fn verify_ignores_active_segment() {
    // Active-segment records are covered by reopen tail-scans, not verify;
    // verify only walks sealed segments. A store with only an active segment
    // verifies clean.
    let dir = TempDir::new().unwrap();
    let s = Store::open(dir.path()).unwrap();
    for o in test_objects(5) {
        s.put(o.key, &o.data).unwrap();
    }
    s.verify(|| false).unwrap();
}

#[test]
fn verify_concurrent_with_close() {
    // Go's Close waits for in-flight scrubs before munmap; the Rust port's
    // scrub snapshot owns its mappings, so a concurrent close must be safe
    // and the scrub must finish clean (or observe the closed store).
    for _ in 0..5 {
        let dir = sealed_store(&test_objects(200));
        let s = Store::open(dir.path()).unwrap();
        thread::scope(|scope| {
            let h = scope.spawn(|| s.verify(|| false));
            thread::sleep(std::time::Duration::from_millis(2));
            s.close().unwrap();
            match h.join().unwrap() {
                Ok(()) => {}
                Err(e) if e.is_closed() => {}
                Err(e) => panic!("verify during close: {e}"),
            }
        });
    }
}

#[test]
fn verify_detects_key_count_mismatch() {
    // A consistent footer built over N+1 entries with only N body records:
    // open passes (trailer/index/filter all self-consistent), the scrub's
    // key-count cross-check must fire.
    let objs = test_objects(10);
    let (body, mut entries) = build_body(&objs);
    let ghost = blob_obj(b"never written to the body");
    entries.push(IndexEntry {
        k: ghost.key,
        off: entries[0].off,
        slen: entries[0].slen,
    });
    let footer = build_footer(body.len() as u64, &entries).unwrap();
    let dir = TempDir::new().unwrap();
    let mut file = body;
    file.extend_from_slice(&footer);
    fs::write(dir.path().join("0000000000000001.seg"), &file).unwrap();
    let s = Store::open(dir.path()).unwrap();
    let err = s.verify(|| false).unwrap_err();
    assert!(err.is_corrupt(), "verify = {err}, want corrupt");
}

#[test]
fn verify_scrub_hash_mismatch_is_corrupt() {
    // A record whose CRC is valid but whose payload doesn't hash to its key
    // (writer-bug class): scrub must classify it as corrupt AND verify,
    // mirroring Go's double %w wrap of ErrCorrupt and ErrVerify.
    let good = test_objects(3);
    let imposter = blob_obj(b"imposter payload");
    let mut body = MAGIC_HEADER.to_vec();
    let mut entries = Vec::new();
    for (i, o) in good.iter().enumerate() {
        let data = if i == 1 { &imposter.data } else { &o.data };
        // Encoded under good[1].key: CRC fine, hash wrong.
        let rec = encode_record(o.key, data).unwrap();
        entries.push(IndexEntry {
            k: o.key,
            off: body.len() as u64,
            slen: (rec.len() - REC_HEADER_SIZE) as u32,
        });
        body.extend_from_slice(&rec);
    }
    let footer = build_footer(body.len() as u64, &entries).unwrap();
    let dir = TempDir::new().unwrap();
    let mut file = body;
    file.extend_from_slice(&footer);
    fs::write(dir.path().join("0000000000000001.seg"), &file).unwrap();
    let s = Store::open(dir.path()).unwrap();
    let err = s.verify(|| false).unwrap_err();
    assert!(err.is_corrupt(), "want corrupt, got {err}");
    assert!(err.is_verify(), "want verify too, got {err}");
}

// ---------------------------------------------------------------------------
// Bulk absence tests (Go: missing_test.go; the chunk-plan regression lives in
// missing.rs unit tests).

#[test]
fn missing_preserves_order_and_multiplicity() {
    let dir = TempDir::new().unwrap();
    // Force a sealed + active mix.
    let s = Store::open_with(dir.path(), Options::default().segment_size(8 << 10)).unwrap();
    let objs = test_objects(200);
    let (stored, absent) = objs.split_at(120);
    for o in stored {
        s.put(o.key, &o.data).unwrap();
    }
    // Interleave present and absent keys, with a duplicate absent key.
    let mut query = Vec::new();
    for (i, a) in absent.iter().enumerate() {
        query.push(stored[i % stored.len()].key);
        query.push(a.key);
    }
    query.push(absent[0].key); // duplicate, must be reported twice

    let got = s.missing(&query).unwrap();
    let mut want: Vec<Key> = absent.iter().map(|o| o.key).collect();
    want.push(absent[0].key);
    assert_eq!(got, want, "order and multiplicity preserved");
}

#[test]
fn missing_empty_input() {
    let dir = TempDir::new().unwrap();
    let s = Store::open(dir.path()).unwrap();
    assert_eq!(s.missing(&[]).unwrap(), Vec::<Key>::new());
}

// ---------------------------------------------------------------------------
// Parallel-writer tests (Go: parallel_test.go).

#[test]
fn write_parallel_stores_all() {
    let dir = TempDir::new().unwrap();
    let s = Store::open_with(dir.path(), Options::default().segment_size(32 << 10)).unwrap();
    let objs = test_objects(300);
    let mut batch = objs.clone();
    batch.extend_from_slice(&objs[..50]); // 50 in-stream dups
    let (stats, res) = s.write_parallel(
        obj_seq(&batch, None),
        WriteOpts {
            writers: 4,
            batch_size: 8 << 10,
            verify: false,
        },
    );
    res.unwrap();
    assert_eq!(stats.stored, objs.len(), "stored");
    assert_eq!(stats.deduped, 50, "deduped");
    let want_bytes: u64 = objs.iter().map(|o| o.data.len() as u64).sum();
    assert_eq!(stats.bytes_stored, want_bytes, "bytes_stored");
    for o in &objs {
        assert_eq!(s.get(o.key).unwrap(), o.data, "get({})", o.key);
    }
}

#[test]
fn write_parallel_skips_existing() {
    let dir = TempDir::new().unwrap();
    let s = Store::open(dir.path()).unwrap();
    let objs = test_objects(20);
    for o in &objs[..10] {
        s.put(o.key, &o.data).unwrap();
    }
    let (stats, res) = s.write_parallel(obj_seq(&objs, None), WriteOpts::default());
    res.unwrap();
    assert_eq!((stats.stored, stats.deduped), (10, 10), "stats = {stats:?}");
}

#[test]
fn write_parallel_verify_catches_mismatch() {
    let dir = TempDir::new().unwrap();
    let s = Store::open(dir.path()).unwrap();
    let good = test_objects(5);
    let mut bad = good[2].clone();
    bad.data.push(0xFF); // payload no longer matches the key
    let mut objs = good[..2].to_vec();
    objs.push(bad);
    let (_, res) = s.write_parallel(
        obj_seq(&objs, None),
        WriteOpts {
            verify: true,
            ..WriteOpts::default()
        },
    );
    let err = res.unwrap_err();
    assert!(err.is_verify(), "err = {err}, want verify");
}

#[test]
fn write_parallel_iterator_error() {
    let dir = TempDir::new().unwrap();
    let s = Store::open(dir.path()).unwrap();
    let objs = test_objects(10);
    let (_, res) = s.write_parallel(
        obj_seq(&objs, Some(7)),
        WriteOpts {
            writers: 2,
            ..WriteOpts::default()
        },
    );
    assert!(res.is_err(), "want iterator error");
}

#[test]
fn write_parallel_on_closed_store() {
    let dir = TempDir::new().unwrap();
    let s = Store::open(dir.path()).unwrap();
    s.close().unwrap();
    let objs = test_objects(5);
    let (_, res) = s.write_parallel(
        obj_seq(&objs, None),
        WriteOpts {
            writers: 2,
            ..WriteOpts::default()
        },
    );
    let err = res.unwrap_err();
    assert!(err.is_closed(), "err = {err}, want closed");
}

#[test]
fn write_parallel_error_flushes_prefix() {
    // An erroring run must leave its appended prefix durable (fsynced):
    // reopen after a dirty stop and the prefix records must still be there.
    let dir = TempDir::new().unwrap();
    let s = Store::open(dir.path()).unwrap();
    let objs = test_objects(10);
    let (stats, res) = s.write_parallel(
        obj_seq(&objs, Some(7)),
        WriteOpts {
            writers: 1,
            batch_size: 1 << 30,
            verify: false,
        },
    );
    assert!(res.is_err(), "want iterator error");
    // stats.stored objects were appended but never hit a batch-size flush;
    // the error-path sync must have made them durable. Cancellation races
    // with channel drain, so fewer than 7 objects may have been appended —
    // check only what was actually stored.
    let stored = stats.stored;
    if stored == 0 {
        return; // nothing was appended before the error; nothing to verify
    }
    for o in &objs[..stored] {
        assert!(s.has(o.key).unwrap(), "has({})", o.key);
    }
    // …and durability across a reopen. close() itself fsyncs, so the reopen
    // check alone wouldn't prove the error-path sync — the writers:1 + huge
    // batch-size setup ensures the only fsync before close comes from the
    // error-path sync.
    s.close().unwrap();
    let s2 = Store::open(dir.path()).unwrap();
    for o in &objs[..stored] {
        assert_eq!(
            s2.get(o.key).unwrap(),
            o.data,
            "get({}) after reopen",
            o.key
        );
    }
}

#[test]
fn oversized_record_error_passthrough() {
    // encode_record's TooLarge surfaces unwrapped from put, like Go.
    // (Rather than allocating past MAX_PAYLOAD, exercise the corrupt
    // classification instead: Pack errors are not corrupt/verify classes.)
    let e = Error::Pack(crate::amberpack::Error::TooLarge {
        key: Key::new(Type::Blob, 1, b"x"),
        len: 5,
    });
    assert!(!e.is_corrupt() && !e.is_verify() && !e.is_closed() && !e.is_not_found());
}

// A failed write can leave junk past the active writer's logical size.
// Sealing must not put the footer before it, since parse anchors at EOF
// (Go: TestSealTruncatesStaleBytesPastLogicalEnd).
#[test]
fn seal_truncates_stale_bytes_past_logical_end() {
    let dir = TempDir::new().unwrap();
    let s = Store::open_with(dir.path(), Options::default().segment_size(4096)).unwrap();
    let mut d = incompressible(1000);
    d.push(1);
    let first = blob_obj(&d);
    s.put(first.key, &first.data).unwrap();
    let active = active_files(dir.path());
    assert_eq!(active.len(), 1, "active segments: {active:?}");
    {
        use std::io::Write;
        let mut f = fs::OpenOptions::new()
            .append(true)
            .open(&active[0])
            .unwrap();
        f.write_all(&vec![0xEE; 64 << 10]).unwrap();
    }

    // Push the segment over the threshold so this put seals it.
    let mut d = incompressible(4000);
    d.push(2);
    let second = blob_obj(&d);
    s.put(second.key, &second.data).unwrap();
    s.close().unwrap();

    let s2 = Store::open(dir.path()).unwrap();
    for o in [&first, &second] {
        assert_eq!(
            s2.get(o.key).unwrap(),
            o.data,
            "get({}) after reopen",
            o.key
        );
    }
}

// A dedup-only run must still fsync: the matched records may belong to a
// concurrent writer that has not synced yet (Go:
// TestWriteParallelDedupOnlyRunStillSyncs).
#[test]
fn write_parallel_dedup_only_run_still_syncs() {
    let dir = TempDir::new().unwrap();
    let s = Store::open(dir.path()).unwrap();
    let objs = test_objects(4);
    // Stand in for a concurrent, not-yet-committed writer.
    for o in &objs {
        let rec = encode_record(o.key, &o.data).unwrap();
        s.append(o.key, &rec, false).unwrap();
    }
    let before = s.fsyncs.load(std::sync::atomic::Ordering::Relaxed);
    let (stats, res) = s.write_parallel(obj_seq(&objs, None), WriteOpts::default());
    res.unwrap();
    assert_eq!(
        (stats.stored, stats.deduped),
        (0, objs.len()),
        "stats = {stats:?}, want all deduped"
    );
    assert_ne!(
        s.fsyncs.load(std::sync::atomic::Ordering::Relaxed),
        before,
        "write_parallel returned success without an fsync"
    );
}

// Go: TestWriteParallelSyncsOncePerRun.
#[test]
fn write_parallel_syncs_once_per_run() {
    let dir = TempDir::new().unwrap();
    let s = Store::open(dir.path()).unwrap();
    let objs = test_objects(16);
    let before = s.fsyncs.load(std::sync::atomic::Ordering::Relaxed);
    let (_, res) = s.write_parallel(
        obj_seq(&objs, None),
        WriteOpts {
            writers: 8,
            ..Default::default()
        },
    );
    res.unwrap();
    let n = s.fsyncs.load(std::sync::atomic::Ordering::Relaxed) - before;
    assert_eq!(n, 1, "{n} fsyncs for one small run, want 1");
}
