//! The pre-encoded record write path: objects offered as complete records
//! through `write_parallel`, `write_batch`, and `append_record` (Go:
//! `record_test.go`).

use tempfile::TempDir;

use crate::amberpack::encode_record;

use super::store_tests::obj_seq;
use super::testutil::*;
use super::{Object, Options, Store, WriteOpts};

/// `o` as a pre-encoded record: what a caller that already holds the record
/// bytes (a staged pack) offers instead of `data` (Go: `recordObj`).
fn record_obj(o: &Object) -> Object {
    Object {
        key: o.key,
        data: Vec::new(),
        record: Some(encode_record(o.key, &o.data).unwrap()),
    }
}

/// Flips the last byte of `o`'s record so its CRC no longer matches.
fn corrupt_record(o: &mut Object) {
    let rec = o.record.as_mut().unwrap();
    let last = rec.len() - 1;
    rec[last] ^= 0x01;
}

#[test]
fn write_parallel_records_stored_and_readable() {
    let dir = TempDir::new().unwrap();
    let s = Store::open_with(dir.path(), Options::default().segment_size(32 << 10)).unwrap();
    let objs = test_objects(60);
    let recs: Vec<Object> = objs.iter().map(record_obj).collect();
    let mut batch = recs.clone();
    batch.extend_from_slice(&recs[..7]); // in-stream dups
    let (stats, res) = s.write_parallel(
        obj_seq(&batch, None),
        WriteOpts {
            writers: 3,
            batch_size: 8 << 10,
            verify: true,
        },
    );
    res.unwrap();
    assert_eq!(
        (stats.stored, stats.deduped),
        (objs.len(), 7),
        "stats = {stats:?}"
    );
    let want_bytes: u64 = objs.iter().map(|o| o.data.len() as u64).sum();
    assert_eq!(
        stats.bytes_stored, want_bytes,
        "bytes_stored (the records' ulen)"
    );
    for (i, o) in objs.iter().enumerate() {
        assert_eq!(s.get(o.key).unwrap(), o.data, "get({})", o.key);
        // The record went in verbatim: the stored bytes are the offered ones.
        let got = s.get_record(o.key).unwrap();
        assert_eq!(
            recs[i].record.as_deref(),
            Some(&got[..]),
            "object {i}: stored record differs from the offered one"
        );
    }
}

#[test]
fn write_parallel_records_dedup_against_present() {
    let dir = TempDir::new().unwrap();
    let s = Store::open(dir.path()).unwrap();
    let objs = test_objects(20);
    for o in &objs[..10] {
        s.put(o.key, &o.data).unwrap();
    }
    let recs: Vec<Object> = objs.iter().map(record_obj).collect();
    let (stats, res) = s.write_parallel(
        obj_seq(&recs, None),
        WriteOpts {
            verify: true,
            ..WriteOpts::default()
        },
    );
    res.unwrap();
    assert_eq!((stats.stored, stats.deduped), (10, 10), "stats = {stats:?}");
}

#[test]
fn write_parallel_record_verify_catches_wrong_payload() {
    // The record is well formed (its CRC is right) but its payload does not
    // hash to its key: only verify can tell, exactly as for data.
    let dir = TempDir::new().unwrap();
    let s = Store::open(dir.path()).unwrap();
    let objs = test_objects(3);
    let mut wrong = objs[0].data.clone();
    wrong.push(0xFF);
    let bad = Object {
        key: objs[0].key,
        data: Vec::new(),
        record: Some(encode_record(objs[0].key, &wrong).unwrap()),
    };
    let (_, res) = s.write_parallel(
        obj_seq(&[record_obj(&objs[1]), bad.clone()], None),
        WriteOpts {
            verify: true,
            ..WriteOpts::default()
        },
    );
    let err = res.unwrap_err();
    assert!(err.is_verify(), "err = {err}, want verify");
    assert!(
        !s.has(objs[0].key).unwrap(),
        "mismatching record was stored"
    );
    // Without verify the record is taken on trust, as data is.
    let (_, res) = s.write_parallel(obj_seq(&[bad], None), WriteOpts::default());
    res.unwrap();
}

#[test]
fn write_parallel_record_corrupt_fails() {
    let dir = TempDir::new().unwrap();
    let s = Store::open(dir.path()).unwrap();
    let mut o = record_obj(&test_objects(1)[0]);
    corrupt_record(&mut o);
    let (_, res) = s.write_parallel(obj_seq(&[o.clone()], None), WriteOpts::default());
    let err = res.unwrap_err();
    assert!(err.is_corrupt(), "err = {err}, want corrupt");
    assert!(!s.has(o.key).unwrap(), "corrupt record was stored");
}

#[test]
fn write_parallel_record_key_mismatch_fails() {
    let dir = TempDir::new().unwrap();
    let s = Store::open(dir.path()).unwrap();
    let objs = test_objects(2);
    let mut o = record_obj(&objs[0]);
    o.key = objs[1].key; // record says objs[0], object says objs[1]
    let (_, res) = s.write_parallel(obj_seq(&[o], None), WriteOpts::default());
    let err = res.unwrap_err();
    assert!(err.is_corrupt(), "err = {err}, want corrupt");
}

#[test]
fn write_parallel_data_and_record_fails() {
    let dir = TempDir::new().unwrap();
    let s = Store::open(dir.path()).unwrap();
    let o = test_objects(1).remove(0);
    let mut both = record_obj(&o);
    both.data = o.data.clone();
    let (_, res) = s.write_parallel(obj_seq(&[both], None), WriteOpts::default());
    let err = res.unwrap_err();
    assert!(err.is_corrupt(), "err = {err}, want corrupt");
}

#[test]
fn write_batch_records() {
    let dir = TempDir::new().unwrap();
    let s = Store::open(dir.path()).unwrap();
    let objs = test_objects(30);
    s.put(objs[0].key, &objs[0].data).unwrap();
    let mut recs: Vec<Object> = objs.iter().map(record_obj).collect();
    recs.push(recs[5].clone()); // in-batch duplicate
    s.write_batch(obj_seq(&recs, None)).unwrap();
    for o in &objs {
        assert_eq!(s.get(o.key).unwrap(), o.data, "get({})", o.key);
    }
    let mut bad = record_obj(&test_objects(40)[39]);
    corrupt_record(&mut bad);
    let err = s.write_batch(obj_seq(&[bad], None)).unwrap_err();
    assert!(
        err.is_corrupt(),
        "write_batch of a corrupt record: err = {err}, want corrupt"
    );
}

/// Go guards `AppendRecord` against a nil raw: its `prepare` would read it as
/// an Object carrying Data instead of a Record and append an empty payload
/// under k, quietly manufacturing an object the GC copier never held. Here
/// the slice is always a record, so an empty one fails the record parse;
/// what is pinned is the class and that nothing is stored (Go:
/// `TestAppendRecordRejectsNilRecord`).
#[test]
fn append_record_rejects_empty_record() {
    let dir = TempDir::new().unwrap();
    let s = Store::open(dir.path()).unwrap();
    let o = test_objects(1).remove(0);
    let err = s.append_record(o.key, &[]).unwrap_err();
    assert!(
        err.is_corrupt(),
        "append_record(k, []): err = {err}, want corrupt"
    );
    assert!(!s.has(o.key).unwrap(), "an empty record stored an object");
}
