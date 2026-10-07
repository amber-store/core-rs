//! Compression options (Go: `packstore/compression_test.go` and
//! `packstore/mixed_codec_test.go`).

use std::collections::HashMap;
use std::fs;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use tempfile::TempDir;

use super::sidecar::SIDECAR_SUFFIX;
use super::store_tests::obj_seq;
use super::testutil::*;
use super::{CompactOpts, Object, Options, Store, WriteOpts};
use crate::amberpack::{Compression, REC_HEADER_SIZE, encode_record_with};
use crate::key::Key;

const LZ4: Compression = Compression::Lz4 { level: 0 };
const LZ4_9: Compression = Compression::Lz4 { level: 9 };
const ZSTD_19: Compression = Compression::Zstd { level: 19 };

const RAW: u8 = 0;
const CODEC_ZSTD: u8 = 1;
const CODEC_LZ4: u8 = 2;

fn codec_id(c: Compression) -> u8 {
    match c {
        Compression::None => RAW,
        Compression::Zstd { .. } => CODEC_ZSTD,
        Compression::Lz4 { .. } => CODEC_LZ4,
    }
}

/// The codec id of `k`'s stored record (Go: `codecOf`).
fn codec_of(s: &Store, k: Key) -> u8 {
    s.get_record(k).unwrap()[33]
}

/// Opens a store with `opts` and nothing else: no compression unless they
/// say so (Go: `rawStore`).
fn raw_store(opts: Options) -> (TempDir, Store) {
    let dir = TempDir::new().unwrap();
    let s = Store::open_with(dir.path(), opts.sync(false)).unwrap();
    (dir, s)
}

/// n compressible objects that differ from one another (Go: `distinct`).
fn distinct(n: usize) -> Vec<Object> {
    (0..n)
        .map(|i| {
            let mut data = compressible(4096);
            data.push(i as u8);
            data.push((i >> 8) as u8);
            blob_obj(&data)
        })
        .collect()
}

fn must_get(s: &Store, o: &Object) {
    assert_eq!(s.get(o.key).unwrap(), o.data, "get({})", o.key);
}

fn parallel(writers: usize) -> WriteOpts {
    WriteOpts {
        writers,
        ..WriteOpts::default()
    }
}

#[test]
fn default_store_writes_raw() {
    let (_dir, s) = raw_store(Options::new());
    let o = blob_obj(&compressible(4096));
    s.put(o.key, &o.data).unwrap();
    assert_eq!(codec_of(&s, o.key), RAW);
    assert_eq!(s.stored_size(o.key).unwrap(), Some(o.data.len() as u64));
    must_get(&s, &o);
}

#[test]
fn compression_on_every_write_path() {
    type Write = fn(&Store, &[Object]);
    let paths: [(&str, Write); 4] = [
        ("put", |s, objs| {
            for o in objs {
                s.put(o.key, &o.data).unwrap();
            }
        }),
        ("put_verified", |s, objs| {
            for o in objs {
                s.put_verified(o.key, &o.data).unwrap();
            }
        }),
        ("write_batch", |s, objs| {
            s.write_batch(obj_seq(objs, None)).unwrap();
        }),
        ("write_parallel", |s, objs| {
            s.write_parallel(obj_seq(objs, None), parallel(4))
                .1
                .unwrap();
        }),
    ];
    for c in [ZSTD, ZSTD_19, LZ4, LZ4_9] {
        for (name, write) in paths {
            let (_dir, s) = raw_store(Options::new().compression(c));
            let objs = distinct(6);
            write(&s, &objs);
            for o in &objs {
                assert_eq!(codec_of(&s, o.key), codec_id(c), "{c}/{name}");
                must_get(&s, o);
            }
        }
    }
}

/// What the callback was handed: the key, the bytes and the store's setting.
type Call = (Key, Vec<u8>, Compression);

#[test]
fn compression_for_receives_key_data_and_default() {
    let calls: Arc<Mutex<Vec<Call>>> = Arc::default();
    let seen = Arc::clone(&calls);
    let (_dir, s) = raw_store(Options::new().compression(LZ4).compression_for(
        move |k, data, def| {
            seen.lock().unwrap().push((*k, data.to_vec(), def));
            def
        },
    ));
    let o = blob_obj(&compressible(4096));
    s.put(o.key, &o.data).unwrap();
    assert_eq!(*calls.lock().unwrap(), vec![(o.key, o.data.clone(), LZ4)]);
    assert_eq!(codec_of(&s, o.key), CODEC_LZ4);
}

#[test]
fn compression_for_chooses_per_object() {
    // The store's setting is lz4. The callback stores "raw…" objects raw,
    // sends "zstd…" objects to zstd 19 and accepts the setting for the rest.
    let (_dir, s) = raw_store(
        Options::new()
            .compression(LZ4)
            .compression_for(|_, data, def| {
                if data.starts_with(b"raw") {
                    Compression::None
                } else if data.starts_with(b"zstd") {
                    ZSTD_19
                } else {
                    def
                }
            }),
    );
    for (prefix, want) in [("raw", RAW), ("zstd", CODEC_ZSTD), ("other", CODEC_LZ4)] {
        let mut data = prefix.as_bytes().to_vec();
        data.extend(compressible(4096));
        let o = blob_obj(&data);
        s.put(o.key, &o.data).unwrap();
        assert_eq!(codec_of(&s, o.key), want, "{prefix} object");
        must_get(&s, &o);
    }
}

#[test]
fn compression_for_is_asked_when_the_setting_is_none() {
    let saw: Arc<Mutex<Option<Compression>>> = Arc::default();
    let seen = Arc::clone(&saw);
    let (_dir, s) = raw_store(Options::new().compression_for(move |_, _, def| {
        *seen.lock().unwrap() = Some(def);
        ZSTD
    }));
    let o = blob_obj(&compressible(4096));
    s.put(o.key, &o.data).unwrap();
    assert_eq!(*saw.lock().unwrap(), Some(Compression::None));
    assert_eq!(codec_of(&s, o.key), CODEC_ZSTD);
}

#[test]
fn compression_for_invalid_value_fails_the_write() {
    let bad = Arc::new(AtomicBool::new(true));
    let flag = Arc::clone(&bad);
    let (_dir, s) = raw_store(Options::new().compression_for(move |_, _, def| {
        if flag.load(Ordering::SeqCst) {
            Compression::Zstd { level: 99 }
        } else {
            def
        }
    }));
    let objs = distinct(8);
    let err = s.put(objs[0].key, &objs[0].data).unwrap_err();
    assert!(err.is_invalid_compression(), "put: {err}");
    assert!(
        err.to_string().contains(&objs[0].key.to_string()),
        "put error {err} does not name the key"
    );
    let err = s.put_verified(objs[0].key, &objs[0].data).unwrap_err();
    assert!(err.is_invalid_compression(), "put_verified: {err}");
    let err = s.write_batch(obj_seq(&objs, None)).unwrap_err();
    assert!(err.is_invalid_compression(), "write_batch: {err}");
    let err = s
        .write_parallel(obj_seq(&objs, None), parallel(4))
        .1
        .unwrap_err();
    assert!(err.is_invalid_compression(), "write_parallel: {err}");
    for o in &objs {
        assert!(
            !s.has(o.key).unwrap(),
            "{} stored by a rejected write",
            o.key
        );
    }
    // The failure is that object's, not the store's: it keeps working.
    bad.store(false, Ordering::SeqCst);
    s.write_parallel(obj_seq(&objs, None), parallel(4))
        .1
        .unwrap();
    for o in &objs {
        must_get(&s, o);
    }
}

#[test]
fn compression_for_is_not_asked_for_dedup_hits_or_records() {
    let calls = Arc::new(AtomicUsize::new(0));
    let n = Arc::clone(&calls);
    let (_dir, s) = raw_store(Options::new().compression(ZSTD).compression_for(
        move |_, _, def| {
            n.fetch_add(1, Ordering::SeqCst);
            def
        },
    ));
    let o = blob_obj(&compressible(4096));
    s.put(o.key, &o.data).unwrap();
    s.put(o.key, &o.data).unwrap();
    s.write_batch(obj_seq(&[o.clone(), o.clone()], None))
        .unwrap();
    assert_eq!(
        calls.load(Ordering::SeqCst),
        1,
        "one new object, three dedup hits"
    );

    // A pre-encoded record is appended as it is and keeps its codec.
    let recs: Vec<Object> = (0..4u8)
        .map(|i| {
            let mut data = compressible(4096);
            data.extend([0xEE, i]);
            let p = blob_obj(&data);
            Object {
                key: p.key,
                data: Vec::new(),
                record: Some(encode_record_with(p.key, &p.data, LZ4).unwrap()),
            }
        })
        .collect();
    s.write_batch(obj_seq(&recs[..2], None)).unwrap();
    s.write_parallel(obj_seq(&recs[2..], None), parallel(2))
        .1
        .unwrap();
    assert_eq!(
        calls.load(Ordering::SeqCst),
        1,
        "pre-encoded records must not reach the callback"
    );
    for r in &recs {
        assert_eq!(codec_of(&s, r.key), CODEC_LZ4);
    }
}

#[test]
fn put_dedups_across_codecs() {
    let dir = TempDir::new().unwrap();
    let o = blob_obj(&compressible(4096));
    let a = Store::open_with(dir.path(), Options::new().sync(false).compression(LZ4)).unwrap();
    a.put(o.key, &o.data).unwrap();
    a.close().unwrap();

    let calls = Arc::new(AtomicUsize::new(0));
    let n = Arc::clone(&calls);
    let b = Store::open_with(
        dir.path(),
        Options::new()
            .sync(false)
            .compression(ZSTD)
            .compression_for(move |_, _, def| {
                n.fetch_add(1, Ordering::SeqCst);
                def
            }),
    )
    .unwrap();
    b.put(o.key, &o.data).unwrap();
    assert_eq!(
        calls.load(Ordering::SeqCst),
        0,
        "the store already holds the object"
    );
    assert_eq!(
        codec_of(&b, o.key),
        CODEC_LZ4,
        "the record written first stays"
    );
}

#[test]
fn compression_for_under_parallel_writers() {
    let calls = Arc::new(AtomicUsize::new(0));
    let n = Arc::clone(&calls);
    let (_dir, s) = raw_store(Options::new().compression(LZ4).compression_for(
        move |_, data, def| {
            n.fetch_add(1, Ordering::SeqCst);
            // distinct's low index byte
            if data[data.len() - 2] % 2 == 0 {
                ZSTD
            } else {
                def
            }
        },
    ));
    let objs = distinct(200);
    let (stats, res) = s.write_parallel(obj_seq(&objs, None), parallel(8));
    res.unwrap();
    assert_eq!(stats.stored, objs.len());
    assert_eq!(calls.load(Ordering::SeqCst), objs.len());
    for (i, o) in objs.iter().enumerate() {
        let want = if i % 2 == 0 { CODEC_ZSTD } else { CODEC_LZ4 };
        assert_eq!(codec_of(&s, o.key), want, "object {i}");
    }
}

#[test]
fn open_rejects_invalid_compression() {
    for c in [
        Compression::Zstd { level: 23 },
        Compression::Zstd { level: -1 },
        Compression::Lz4 { level: 13 },
    ] {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path().join("store");
        let err =
            Store::open_with(&dir, Options::new().compression(c)).expect_err("open must fail");
        assert!(err.is_invalid_compression(), "{c:?}: {err}");
        assert!(
            !dir.exists(),
            "{c:?}: open created the directory before rejecting the option"
        );
    }
}

#[test]
fn the_last_compression_wins() {
    let (_dir, s) = raw_store(Options::new().compression(ZSTD).compression(LZ4));
    let o = blob_obj(&compressible(4096));
    s.put(o.key, &o.data).unwrap();
    assert_eq!(codec_of(&s, o.key), CODEC_LZ4);
}

#[test]
fn tiny_objects_round_trip() {
    let sizes = [0usize, 1, 2, 12, 13, 64];
    for c in [ZSTD, LZ4, LZ4_9] {
        let seen: Arc<Mutex<Vec<usize>>> = Arc::default();
        let log = Arc::clone(&seen);
        let (_dir, s) = raw_store(Options::new().compression(c).compression_for(
            move |_, data, def| {
                log.lock().unwrap().push(data.len());
                def
            },
        ));
        for n in sizes {
            let o = blob_obj(&vec![b'a'; n]);
            s.put(o.key, &o.data).unwrap();
            must_get(&s, &o);
        }
        assert_eq!(*seen.lock().unwrap(), sizes, "{c}");
        // verify re-parses every record: the length invariants hold.
        s.verify(|| false).unwrap();
    }
}

#[test]
fn repair_replacement_uses_the_handles_compression() {
    let objs = test_objects(4); // even indexes compress
    let (dir, path, entries) = write_sealed_file(&objs); // zstd records
    let mut bytes = fs::read(&path).unwrap();
    bytes[entries[0].off as usize + REC_HEADER_SIZE] ^= 0x40;
    fs::write(&path, bytes).unwrap();

    let s = Store::open_with(dir.path(), Options::new().compression(LZ4)).unwrap();
    assert!(s.verify(|| false).is_err());
    s.put_verified(objs[0].key, &objs[0].data).unwrap();
    assert_eq!(
        codec_of(&s, objs[0].key),
        CODEC_LZ4,
        "the replacement uses the handle's setting"
    );
    assert_eq!(
        codec_of(&s, objs[2].key),
        CODEC_ZSTD,
        "its neighbours keep their codec"
    );
    must_get(&s, &objs[0]);
    s.verify(|| false).unwrap();
}

/// Fills one store through five handles, each with its own compression, and
/// reads it through a handle with the default: after a plain reopen, after
/// losing every sidecar, and after a compaction (Go: `TestMixedCodecStore`).
#[test]
fn mixed_codec_store() {
    let dir = TempDir::new().unwrap();
    let small = || Options::new().segment_size(8 << 10).sync(false);
    let mut objs = Vec::new();
    for (si, c) in [Compression::None, ZSTD, LZ4, ZSTD_19, LZ4_9]
        .into_iter()
        .enumerate()
    {
        let s = Store::open_with(dir.path(), small().compression(c)).unwrap();
        for i in 0..12u8 {
            let mut data = if i % 3 == 0 {
                incompressible(3000)
            } else {
                compressible(3000)
            };
            data.extend([si as u8, i]);
            let o = blob_obj(&data);
            s.put(o.key, &o.data).unwrap();
            objs.push(o);
        }
        s.close().unwrap();
    }
    let read_all = |s: &Store| -> HashMap<Key, u8> {
        objs.iter()
            .map(|o| {
                must_get(s, o);
                (o.key, codec_of(s, o.key))
            })
            .collect()
    };

    let s = Store::open_with(dir.path(), small()).unwrap();
    let before = read_all(&s);
    for codec in [RAW, CODEC_ZSTD, CODEC_LZ4] {
        assert!(
            before.values().any(|&c| c == codec),
            "the store holds no record of codec {codec}"
        );
    }
    s.verify(|| false).unwrap();
    s.close().unwrap();

    // Without their sidecars the active segments are indexed by scanning them.
    let sidecars: Vec<_> = fs::read_dir(dir.path())
        .unwrap()
        .map(|e| e.unwrap().path())
        .filter(|p| p.to_string_lossy().ends_with(SIDECAR_SUFFIX))
        .collect();
    assert!(!sidecars.is_empty(), "want at least one sidecar");
    for p in sidecars {
        fs::remove_file(p).unwrap();
    }
    let s = Store::open_with(dir.path(), small()).unwrap();
    assert_eq!(
        read_all(&s),
        before,
        "codecs changed across a reopen without sidecars"
    );
    s.verify(|| false).unwrap();

    // Compaction copies the live records as they are.
    let live = |k: Key| k.as_bytes()[0].is_multiple_of(2);
    s.compact(live, CompactOpts::default()).unwrap();
    for o in objs.iter().filter(|o| live(o.key)) {
        must_get(&s, o);
        assert_eq!(
            codec_of(&s, o.key),
            before[&o.key],
            "compaction changed {}",
            o.key
        );
    }
    s.verify(|| false).unwrap();
}

/// The callback rejects one object in the middle of a run: what was written
/// before it stays, the rejected object is absent, and the store keeps
/// working (Go: `TestCompressionForInvalidValueMidRun`).
#[test]
fn compression_for_invalid_value_mid_run() {
    const REJECT: usize = 20;
    let objs = distinct(40);
    let open = |bad: &Arc<AtomicBool>| {
        let flag = Arc::clone(bad);
        raw_store(
            Options::new()
                .compression(ZSTD)
                .compression_for(move |_, data, def| {
                    // distinct's low index byte
                    if flag.load(Ordering::SeqCst) && data[data.len() - 2] as usize == REJECT {
                        Compression::Lz4 { level: 99 }
                    } else {
                        def
                    }
                }),
        )
    };
    let bad = Arc::new(AtomicBool::new(true));

    // A batch keeps exactly the objects before the rejected one.
    let (_dir, s) = open(&bad);
    let err = s.write_batch(obj_seq(&objs, None)).unwrap_err();
    assert!(err.is_invalid_compression(), "write_batch: {err}");
    assert!(
        err.to_string().contains(&objs[REJECT].key.to_string()),
        "write_batch error {err} does not name the rejected object"
    );
    for (i, o) in objs.iter().enumerate() {
        assert_eq!(s.has(o.key).unwrap(), i < REJECT, "object {i}");
    }
    for o in &objs[..REJECT] {
        must_get(&s, o);
    }

    // A parallel run stops, the rejected object is absent, what it stored
    // reads back, and the store is not poisoned.
    let (_dir, s) = open(&bad);
    let (stats, res) = s.write_parallel(obj_seq(&objs, None), parallel(4));
    let err = res.unwrap_err();
    assert!(err.is_invalid_compression(), "write_parallel: {err}");
    let mut stored = 0;
    for (i, o) in objs.iter().enumerate() {
        let has = s.has(o.key).unwrap();
        assert!(!(i == REJECT && has), "the rejected object was stored");
        if has {
            stored += 1;
            must_get(&s, o);
        }
    }
    assert_eq!(stored, stats.stored, "the stats and the store disagree");
    s.verify(|| false).unwrap();
    bad.store(false, Ordering::SeqCst);
    s.write_parallel(obj_seq(&objs, None), parallel(4))
        .1
        .unwrap();
    for o in &objs {
        must_get(&s, o);
    }
}

/// "name size" for every entry of `dir`, sorted (Go: `listDir`).
fn list_dir(dir: &std::path::Path) -> Vec<String> {
    let mut out: Vec<String> = fs::read_dir(dir)
        .unwrap()
        .map(|e| {
            let e = e.unwrap();
            format!(
                "{} {}",
                e.file_name().to_string_lossy(),
                e.metadata().unwrap().len()
            )
        })
        .collect();
    out.sort();
    out
}

/// A repair whose replacement cannot be encoded fails before it changes
/// anything on disk: the active segment stays active, and the repair goes
/// through once the callback behaves (Go:
/// `TestRejectedRepairLeavesTheStoreUntouched`).
#[test]
fn rejected_repair_leaves_the_store_untouched() {
    let objs = test_objects(4);
    let (dir, path, entries) = write_sealed_file(&objs);
    let mut bytes = fs::read(&path).unwrap();
    bytes[entries[0].off as usize + REC_HEADER_SIZE] ^= 0x40;
    fs::write(&path, bytes).unwrap();

    let bad = Arc::new(AtomicBool::new(false));
    let flag = Arc::clone(&bad);
    let s = Store::open_with(
        dir.path(),
        Options::new()
            .sync(false)
            .compression(ZSTD)
            .compression_for(move |_, _, def| {
                if flag.load(Ordering::SeqCst) {
                    Compression::Zstd { level: 99 }
                } else {
                    def
                }
            }),
    )
    .unwrap();
    let later = blob_obj(b"a later object, in a new active segment");
    s.put(later.key, &later.data).unwrap();

    let before = list_dir(dir.path());
    bad.store(true, Ordering::SeqCst);
    let err = s.put_verified(objs[0].key, &objs[0].data).unwrap_err();
    assert!(err.is_invalid_compression(), "put_verified: {err}");
    assert_eq!(
        list_dir(dir.path()),
        before,
        "a rejected repair changed the store directory"
    );

    bad.store(false, Ordering::SeqCst);
    s.put_verified(objs[0].key, &objs[0].data).unwrap();
    must_get(&s, &objs[0]);
    must_get(&s, &later);
    s.verify(|| false).unwrap();
}

/// A deferred put that the callback rejects wrote nothing, so it leaves no
/// unsynced-record flag behind for the next dedup hit to pay for.
#[test]
fn rejected_deferred_put_sets_no_flag() {
    let (_dir, s) =
        raw_store(Options::new().compression_for(|_, _, _| Compression::Zstd { level: 99 }));
    let o = blob_obj(&compressible(4096));
    let err = s.put_verified_deferred(o.key, &o.data).unwrap_err();
    assert!(err.is_invalid_compression(), "{err}");
    assert!(!s.deferred.load(Ordering::SeqCst));
    assert!(!s.has(o.key).unwrap());
}

/// A run whose only writer fails must still return. The distributor can be
/// blocked sending into a full channel at that moment, and nothing wakes it
/// if the writers simply leave. Run on its own thread so that a regression
/// fails the test rather than hanging the suite.
#[test]
fn write_parallel_returns_when_its_writer_fails_with_the_channel_full() {
    let (_dir, s) =
        raw_store(Options::new().compression_for(|_, _, _| Compression::Zstd { level: 99 }));
    let s = Arc::new(s);
    let objs = distinct(64);
    let (tx, rx) = std::sync::mpsc::channel();
    let store = Arc::clone(&s);
    std::thread::spawn(move || {
        let (_, res) = store.write_parallel(obj_seq(&objs, None), parallel(1));
        let _ = tx.send(res.map_err(|e| e.is_invalid_compression()));
    });
    match rx.recv_timeout(std::time::Duration::from_secs(20)) {
        Ok(res) => assert_eq!(res, Err(true), "want an invalid-compression error"),
        Err(_) => panic!(
            "write_parallel did not return: the distributor is stuck sending to writers that gave up"
        ),
    }
}
