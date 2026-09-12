//! Port of Go `refstore/refstore_test.go`, plus a byte-exact round-trip of
//! canonical reference records through the store.

use std::sync::{Arc, Barrier};

use amber_store_core::refstore::Store;
use amber_store_core::{key, reference};

fn open(dir: &std::path::Path) -> Store {
    Store::open(dir, false).expect("open refstore")
}

// Port of Go TestPutGetDelete.
#[test]
fn put_get_delete() {
    let dir = tempfile::tempdir().unwrap();
    let s = open(dir.path());

    let err = s.get("missing").unwrap_err();
    assert!(err.is_not_found(), "get(missing) = {err}, want NotFound");
    s.put("a/b", b"rec1").unwrap();
    assert_eq!(s.get("a/b").unwrap(), b"rec1");
    // Overwrite is unconditional.
    s.put("a/b", b"rec2").unwrap();
    assert_eq!(s.get("a/b").unwrap(), b"rec2");
    s.delete("a/b").unwrap();
    let err = s.get("a/b").unwrap_err();
    assert!(
        err.is_not_found(),
        "get after delete = {err}, want NotFound"
    );
    let err = s.delete("a/b").unwrap_err();
    assert!(err.is_not_found(), "delete(absent) = {err}, want NotFound");
}

// Port of Go TestAllSortedByName.
#[test]
fn all_sorted_by_name() {
    let dir = tempfile::tempdir().unwrap();
    let s = open(dir.path());
    for n in ["zeta", "alpha", "mid/dle"] {
        s.put(n, format!("v-{n}").as_bytes()).unwrap();
    }
    let recs = s.all().unwrap();
    let want_names = ["alpha", "mid/dle", "zeta"];
    assert_eq!(recs.len(), want_names.len());
    for (rec, want) in recs.iter().zip(want_names) {
        assert_eq!(rec.name, want);
        assert_eq!(rec.data, format!("v-{want}").into_bytes());
    }
}

// Port of Go TestSurvivesReopen.
#[test]
fn survives_reopen() {
    let dir = tempfile::tempdir().unwrap();
    let s = Store::open(dir.path(), false).unwrap();
    s.put("keep", b"v").unwrap();
    drop(s); // Close
    let s2 = open(dir.path());
    assert_eq!(s2.get("keep").unwrap(), b"v");
}

// Port of Go TestAllEmpty.
#[test]
fn all_empty() {
    let dir = tempfile::tempdir().unwrap();
    let s = open(dir.path());
    let recs = s.all().unwrap();
    assert!(
        recs.is_empty(),
        "all on empty store = {} records",
        recs.len()
    );
}

// Port of Go TestConcurrentDeleteReportsOnce.
#[test]
fn concurrent_delete_reports_once() {
    let dir = tempfile::tempdir().unwrap();
    let s = Arc::new(open(dir.path()));
    s.put("target", b"data").unwrap();

    const N: usize = 8;
    // A barrier so all threads start as close together as possible.
    let gate = Arc::new(Barrier::new(N));
    let mut handles = Vec::with_capacity(N);
    for _ in 0..N {
        let s = Arc::clone(&s);
        let gate = Arc::clone(&gate);
        handles.push(std::thread::spawn(move || {
            gate.wait();
            s.delete("target")
        }));
    }
    let mut ok_count = 0;
    for h in handles {
        match h.join().unwrap() {
            Ok(()) => ok_count += 1,
            Err(e) if e.is_not_found() => {} // expected for all-but-one
            Err(e) => panic!("unexpected error: {e}"),
        }
    }
    assert_eq!(ok_count, 1, "exactly 1 delete should succeed");
}

// Port of Go TestWipe.
#[test]
fn wipe() {
    let dir = tempfile::tempdir().unwrap();
    let s = open(dir.path());
    for n in ["a", "b", "c"] {
        s.put(n, format!("v-{n}").as_bytes()).unwrap();
    }
    s.wipe().unwrap();
    let recs = s.all().unwrap();
    assert!(recs.is_empty(), "all after wipe: {} records", recs.len());
    let err = s.get("a").unwrap_err();
    assert!(err.is_not_found(), "get after wipe = {err}, want NotFound");
    // The store stays usable.
    s.put("d", b"v-d").unwrap();
    assert_eq!(s.get("d").unwrap(), b"v-d");
}

// Differential check against Go/Pebble (adversarial review): iteration is
// pure BYTEWISE order — multi-byte UTF-8 sorts after ASCII, embedded NUL
// sorts before longer prefixes, uppercase before lowercase, empty name first.
// Pebble output for these names (hex): "" 41 5a 61 610062 6162 7a 7f c3a9
// c3a97a efbfbf.
#[test]
fn all_orders_names_bytewise() {
    let dir = tempfile::tempdir().unwrap();
    let s = open(dir.path());
    let names = [
        "z", "", "é", "A", "a\u{0}b", "ab", "\u{7f}", "Z", "a", "éz", "\u{ffff}",
    ];
    for n in names {
        s.put(n, b"v").unwrap();
    }
    let got: Vec<String> = s.all().unwrap().into_iter().map(|r| r.name).collect();
    let want = [
        "", "A", "Z", "a", "a\u{0}b", "ab", "z", "\u{7f}", "é", "éz", "\u{ffff}",
    ];
    assert_eq!(got, want, "must match Pebble's bytewise comparer order");
}

// Differential check against Go/Pebble: Wipe on an empty store succeeds, and
// wiping twice in a row succeeds (both return nil in Go).
#[test]
fn wipe_empty_and_twice() {
    let dir = tempfile::tempdir().unwrap();
    let s = open(dir.path());
    s.wipe().unwrap();
    s.put("a", b"v").unwrap();
    s.wipe().unwrap();
    s.wipe().unwrap();
    assert!(s.all().unwrap().is_empty());
}

// Canonical reference records round-trip byte-exactly: the store returns
// exactly the bytes it was given, so decode succeeds (a single non-canonical
// byte would be rejected by reference::Reference::decode).
#[test]
fn reference_records_round_trip_byte_exactly() {
    let dir = tempfile::tempdir().unwrap();
    let s = open(dir.path());

    let key = key::Key::new(key::Type::Blob, 5, b"hello").0.to_vec();
    let records = [
        reference::Reference {
            name: "backups/home".into(),
            key: key.clone(),
            user: "dragan@netice9.com".into(),
            created_at: 1_765_432_100_123_456_789,
            signature: vec![1, 2, 3],
            public_key: vec![4, 5, 6],
        },
        reference::Reference {
            name: "minimal".into(),
            key,
            user: String::new(),
            created_at: -1,
            signature: Vec::new(),
            public_key: Vec::new(),
        },
    ];
    for r in &records {
        let enc = r.encode().unwrap();
        s.put(&r.name, &enc).unwrap();
        let got = s.get(&r.name).unwrap();
        assert_eq!(got, enc, "stored bytes must come back verbatim: {}", r.name);
        assert_eq!(&reference::Reference::decode(&got).unwrap(), r);
    }
    // And through all(): verbatim bytes in name order.
    let recs = s.all().unwrap();
    assert_eq!(recs.len(), records.len());
    assert_eq!(recs[0].name, "backups/home");
    assert_eq!(recs[1].name, "minimal");
    for rec in &recs {
        let r = records.iter().find(|r| r.name == rec.name).unwrap();
        assert_eq!(rec.data, r.encode().unwrap());
    }
}

#[test]
fn batch_replaces_records_and_survives_reopen() {
    use amber_store_core::refstore::Record;
    for sync in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path(), sync).unwrap();
        store.put("a", b"old").unwrap();
        store.put_batch(&[]).unwrap();
        store
            .put_batch(&[
                Record {
                    name: "a".into(),
                    data: b"first".to_vec(),
                },
                Record {
                    name: "\0binary".into(),
                    data: vec![0, 255, 1],
                },
                Record {
                    name: "a".into(),
                    data: b"last".to_vec(),
                },
            ])
            .unwrap();
        drop(store);
        let store = Store::open(dir.path(), sync).unwrap();
        assert_eq!(store.get("a").unwrap(), b"last");
        assert_eq!(store.get("\0binary").unwrap(), vec![0, 255, 1]);
        assert_eq!(store.all().unwrap().len(), 2);
    }
}

#[test]
fn batch_is_visible_atomically_to_readers() {
    use amber_store_core::refstore::Record;
    let dir = tempfile::tempdir().unwrap();
    let store = Arc::new(open(dir.path()));
    let records = |value| {
        (0..20)
            .map(|i| Record {
                name: format!("name-{i:02}"),
                data: vec![value],
            })
            .collect::<Vec<_>>()
    };
    store.put_batch(&records(0)).unwrap();
    let gate = Barrier::new(2);
    std::thread::scope(|scope| {
        scope.spawn(|| {
            gate.wait();
            for value in 1..100 {
                store.put_batch(&records(value)).unwrap();
            }
        });
        gate.wait();
        for _ in 0..200 {
            let snapshot = store.all().unwrap();
            assert_eq!(snapshot.len(), 20);
            assert!(snapshot.iter().all(|r| r.data == snapshot[0].data));
        }
    });
}
