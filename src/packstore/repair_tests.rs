use std::fs;
use std::sync::Arc;

use super::footer::IndexEntry;
use super::testutil::*;
use super::{Error, Store};
use crate::amberpack::REC_HEADER_SIZE;

fn damage(path: &std::path::Path, entry: &IndexEntry, header: bool) {
    let mut bytes = fs::read(path).unwrap();
    bytes[entry.off as usize + if header { 0 } else { REC_HEADER_SIZE }] ^= 0x40;
    fs::write(path, bytes).unwrap();
}

#[test]
fn verified_put_rejects_bad_input_and_preserves_existing_object() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).unwrap();
    let obj = blob_obj(b"correct");
    assert!(store.put_verified(obj.key, b"wrong").is_err());
    assert!(!store.has(obj.key).unwrap());
    store.put_verified(obj.key, &obj.data).unwrap();
    assert!(store.put_verified(obj.key, b"wrong").is_err());
    store.put_verified(obj.key, &obj.data).unwrap();
    assert_eq!(store.get(obj.key).unwrap(), obj.data);
    store.close().unwrap();
    let reopened = Store::open(dir.path()).unwrap();
    assert_eq!(reopened.get(obj.key).unwrap(), obj.data);
    reopened.close().unwrap();
    assert!(matches!(
        reopened.put_verified(obj.key, &obj.data),
        Err(Error::Closed)
    ));
}

#[test]
fn verified_put_repairs_sealed_records_and_preserves_neighbors() {
    for header in [false, true] {
        let objs = test_objects(4);
        let (dir, path, entries) = write_sealed_file(&objs);
        damage(&path, &entries[1], header);
        let store = Store::open(dir.path()).unwrap();
        assert!(store.verify(|| false).is_err());
        store.put_verified(objs[1].key, &objs[1].data).unwrap();
        store.verify(|| false).unwrap();
        assert!(path.exists());
        store.close().unwrap();
        let reopened = Store::open(dir.path()).unwrap();
        reopened.verify(|| false).unwrap();
        for obj in &objs {
            assert_eq!(reopened.get(obj.key).unwrap(), obj.data);
        }
    }
}

#[test]
fn verified_put_repairs_hidden_duplicate_and_keeps_old_mapping_alive() {
    let objs = test_objects(3);
    let (dir, path, entries) = write_sealed_file(&objs);
    fs::copy(&path, dir.path().join("0000000000000002.seg")).unwrap();
    damage(&path, &entries[0], false);
    let store = Store::open(dir.path()).unwrap();
    let old = super::unpoison(store.shared.read()).sealed[0].clone();
    assert_eq!(store.get(objs[0].key).unwrap(), objs[0].data);
    assert!(store.verify(|| false).is_err());
    store.put_verified(objs[0].key, &objs[0].data).unwrap();
    store.verify(|| false).unwrap();
    assert!(old.verify(&|| false).is_err());
    assert_eq!(old.get(objs[1].key).unwrap().unwrap(), objs[1].data);
    store.close().unwrap();
    Store::open(dir.path()).unwrap().verify(|| false).unwrap();
}

#[test]
fn verified_put_repairs_active_corruption_without_losing_later_records() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).unwrap();
    let objs = test_objects(4);
    for obj in &objs {
        store.put(obj.key, &obj.data).unwrap();
    }
    let (_, entries) = build_body(&objs);
    damage(
        &dir.path().join("0000000000000001.seg.active"),
        &entries[1],
        true,
    );
    store.put_verified(objs[1].key, &objs[1].data).unwrap();
    store.verify(|| false).unwrap();
    store.close().unwrap();
    let reopened = Store::open(dir.path()).unwrap();
    for obj in &objs {
        assert_eq!(reopened.get(obj.key).unwrap(), obj.data);
    }
}

#[test]
fn crashed_seal_retains_records_after_corruption() {
    let objs = test_objects(3);
    let (dir, path, entries) = write_sealed_file(&objs);
    damage(&path, &entries[0], true);
    fs::rename(&path, path.with_extension("seg.active")).unwrap();
    let store = Store::open(dir.path()).unwrap();
    assert_eq!(store.get(objs[2].key).unwrap(), objs[2].data);
    assert!(store.verify(|| false).is_err());
    store.put_verified(objs[0].key, &objs[0].data).unwrap();
    store.verify(|| false).unwrap();
    store.close().unwrap();
    Store::open(dir.path()).unwrap().verify(|| false).unwrap();
}

#[test]
fn concurrent_verified_writes_and_reads_preserve_objects() {
    let objs = test_objects(4);
    let (dir, path, entries) = write_sealed_file(&objs);
    damage(&path, &entries[0], false);
    let store = Arc::new(Store::open(dir.path()).unwrap());
    std::thread::scope(|scope| {
        for _ in 0..4 {
            scope.spawn(|| {
                for _ in 0..20 {
                    store.put_verified(objs[0].key, &objs[0].data).unwrap();
                    assert_eq!(store.get(objs[0].key).unwrap(), objs[0].data);
                }
            });
            scope.spawn(|| {
                for _ in 0..20 {
                    assert_eq!(store.get(objs[1].key).unwrap(), objs[1].data);
                }
            });
        }
    });
    store.verify(|| false).unwrap();
}

#[test]
fn verified_put_repairs_hash_mismatch_with_valid_crc() {
    let obj = blob_obj(b"correct");
    let wrong = super::Object {
        key: obj.key,
        data: b"WRONG!!".to_vec(),
        record: None,
    };
    let (dir, _, _) = write_sealed_file(&[wrong]);
    let store = Store::open(dir.path()).unwrap();
    assert!(store.verify(|| false).is_err());
    store.put_verified(obj.key, &obj.data).unwrap();
    store.verify(|| false).unwrap();
    assert_eq!(store.get(obj.key).unwrap(), obj.data);
}

#[test]
fn verified_put_repairs_every_corrupt_duplicate() {
    let objs = test_objects(2);
    let (dir, path, entries) = write_sealed_file(&objs);
    damage(&path, &entries[0], false);
    fs::copy(&path, dir.path().join("0000000000000002.seg")).unwrap();
    let store = Store::open(dir.path()).unwrap();
    store.put_verified(objs[0].key, &objs[0].data).unwrap();
    store.verify(|| false).unwrap();
}

#[test]
fn verified_put_observes_gc_barrier_and_preserves_mark_positions() {
    let objs = test_objects(3);
    let (dir, path, entries) = write_sealed_file(&objs);
    damage(&path, &entries[0], true);
    let store = Store::open(dir.path()).unwrap();
    let mut marks = store.new_mark_set();
    for obj in &objs {
        assert!(marks.mark(obj.key).1);
    }
    store.begin_barrier();
    store.put_verified(objs[0].key, &objs[0].data).unwrap();
    assert!(store.oldest_inflight_write().is_none());
    for obj in &objs {
        assert!(marks.contains(obj.key));
    }
    store
        .compact(|_| false, super::CompactOpts::default())
        .unwrap();
    assert_eq!(store.get(objs[0].key).unwrap(), objs[0].data);
}

#[test]
fn verified_put_rejects_failed_store_and_propagates_active_read_error() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).unwrap();
    let obj = blob_obj(b"hello");
    store.put(obj.key, &obj.data).unwrap();
    let path = dir.path().join("0000000000000001.seg.active");
    fs::OpenOptions::new()
        .write(true)
        .open(path)
        .unwrap()
        .set_len(8)
        .unwrap();
    assert!(matches!(
        store.put_verified(obj.key, &obj.data),
        Err(Error::Io(_))
    ));
    store.set_failed(&"injected failure");
    assert!(matches!(
        store.put_verified(obj.key, &obj.data),
        Err(Error::Failed(_))
    ));
}

#[test]
fn verified_put_cleans_stale_repair_file() {
    let objs = test_objects(2);
    let (dir, path, entries) = write_sealed_file(&objs);
    damage(&path, &entries[0], false);
    let temporary = path.with_extension("repair");
    fs::write(&temporary, b"interrupted replacement").unwrap();
    let store = Store::open(dir.path()).unwrap();
    store.put_verified(objs[0].key, &objs[0].data).unwrap();
    assert!(!temporary.exists());
    store.verify(|| false).unwrap();
}
