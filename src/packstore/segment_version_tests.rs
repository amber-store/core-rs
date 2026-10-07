//! The gate: a record beyond zstd is only ever in a version-3 segment (Go:
//! `packstore/segment_version_test.go`).

use std::collections::HashMap;
use std::fs;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use tempfile::TempDir;

use super::compression_tests::{CODEC_LZ4, CODEC_ZSTD, LZ4, ZSTD_19, distinct, must_get};
use super::footer::{IndexEntry, build_footer};
use super::store_tests::{active_files, obj_seq, sealed_files};
use super::testutil::*;
use super::{CompactOpts, MAGIC_HEADER, Object, Options, Store};
use crate::amberpack::{Compression, REC_HEADER_SIZE, encode_record_with, parse_record};
use crate::key::Key;

/// Reads a segment file, sealed or active, and returns the version byte of
/// its header and the codec of each record, in file order (Go:
/// `segmentRecords`).
fn segment_records(path: &Path) -> (u8, Vec<u8>) {
    let b = fs::read(path).unwrap();
    assert!(
        b.len() >= 8 && &b[..7] == b"AMBERSG",
        "{}: no segment header",
        path.display()
    );
    let mut codecs = Vec::new();
    let mut off = 8;
    // A record starts with its tag, 0x01; a footer starts with another byte.
    while off < b.len() && b[off] == 0x01 {
        let rec = parse_record(&b[off..])
            .unwrap_or_else(|e| panic!("{}: record at offset {off}: {e}", path.display()));
        codecs.push(rec.flags);
        off += REC_HEADER_SIZE + rec.slen as usize;
    }
    (b[7], codecs)
}

/// What [`segment_versions`] reports for one format version.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
struct VersionCount {
    /// Segment files at this version, sealed and active.
    segments: usize,
    /// Records beyond zstd that they hold.
    lz4: usize,
}

fn count(segments: usize, lz4: usize) -> VersionCount {
    VersionCount { segments, lz4 }
}

/// Walks every segment in `dir` and checks the gate's rule: a version-2
/// segment holds no record beyond zstd. Returns the counts per version (Go:
/// `segmentVersions`).
fn segment_versions(dir: &Path) -> HashMap<u8, VersionCount> {
    let mut out: HashMap<u8, VersionCount> = HashMap::new();
    for p in sealed_files(dir).into_iter().chain(active_files(dir)) {
        let (v, codecs) = segment_records(&p);
        let c = out.entry(v).or_default();
        c.segments += 1;
        for codec in codecs {
            if codec <= CODEC_ZSTD {
                continue;
            }
            c.lz4 += 1;
            assert_ne!(
                v,
                2,
                "{} is a version-2 segment and holds a codec-{codec} record",
                p.display()
            );
        }
    }
    out
}

fn at(got: &HashMap<u8, VersionCount>, v: u8) -> VersionCount {
    got.get(&v).copied().unwrap_or_default()
}

/// Options with a callback that picks lz4 while `on` is set and the store's
/// setting otherwise (Go: `switchable`).
fn switchable(on: &Arc<AtomicBool>) -> Options {
    let on = Arc::clone(on);
    Options::new()
        .sync(false)
        .compression(ZSTD)
        .compression_for(move |_, _, def| if on.load(Ordering::SeqCst) { LZ4 } else { def })
}

#[test]
fn segments_stay_at_version_2_without_lz4() {
    for c in [Compression::None, ZSTD, ZSTD_19] {
        let dir = TempDir::new().unwrap();
        let s = Store::open_with(
            dir.path(),
            Options::new()
                .segment_size(8 << 10)
                .sync(false)
                .compression(c),
        )
        .unwrap();
        put_all(&s, &test_objects(40));
        s.close().unwrap();
        let got = segment_versions(dir.path());
        assert!(got.len() == 1 && at(&got, 2).segments >= 2, "{c}: {got:?}");
    }
}

#[test]
fn lz4_handle_writes_version_3_from_the_start() {
    let dir = TempDir::new().unwrap();
    let objs = test_objects(40);
    let s = Store::open_with(
        dir.path(),
        Options::new()
            .segment_size(8 << 10)
            .sync(false)
            .compression(LZ4),
    )
    .unwrap();
    put_all(&s, &objs);
    s.close().unwrap();
    let got = segment_versions(dir.path());
    assert!(
        got.len() == 1 && at(&got, 3).segments >= 2 && at(&got, 3).lz4 > 0,
        "want several segments, all at version 3, with lz4 records: {got:?}"
    );
    // A handle on the default reads and scrubs them.
    let r = Store::open(dir.path()).unwrap();
    for o in &objs {
        must_get(&r, o);
    }
    r.verify(|| false).unwrap();
}

#[test]
fn first_lz4_record_moves_the_handle_to_version_3() {
    let lz4_on = Arc::new(AtomicBool::new(false));
    let dir = TempDir::new().unwrap();
    let s = Store::open_with(dir.path(), switchable(&lz4_on)).unwrap();
    let objs = distinct(5);
    let put = |o: &Object| s.put(o.key, &o.data).unwrap();
    let check = |when: &str, v2: VersionCount, v3: VersionCount| {
        let got = segment_versions(dir.path());
        assert!(
            at(&got, 2) == v2 && at(&got, 3) == v3 && got.len() <= 2,
            "{when}: {got:?}, want version 2 {v2:?} and version 3 {v3:?}"
        );
    };

    put(&objs[0]);
    put(&objs[1]);
    check("two zstd records", count(1, 0), count(0, 0));

    lz4_on.store(true, Ordering::SeqCst);
    put(&objs[2]);
    check("the first lz4 record", count(1, 0), count(1, 1));
    assert_eq!(
        sealed_files(dir.path()).len(),
        1,
        "the version-2 segment the handle left"
    );

    // A zstd record after that goes on in the version-3 segment.
    lz4_on.store(false, Ordering::SeqCst);
    put(&objs[3]);
    check("a zstd record after it", count(1, 0), count(1, 1));

    // And the handle's next segment is at version 3 as well, whatever it
    // starts with: it pays for one early seal, not for one per segment.
    {
        let mut ap = s.append_lock();
        s.seal_active(&mut ap).unwrap();
    }
    put(&objs[4]);
    check("the next segment", count(1, 0), count(2, 1));

    for o in &objs {
        must_get(&s, o);
    }
    s.verify(|| false).unwrap();
}

#[test]
fn pre_encoded_lz4_record_moves_the_handle_to_version_3() {
    let dir = TempDir::new().unwrap();
    let s = Store::open_with(dir.path(), Options::new().sync(false).compression(ZSTD)).unwrap();
    let objs = distinct(2);
    s.put(objs[0].key, &objs[0].data).unwrap();
    let rec = Object {
        key: objs[1].key,
        data: Vec::new(),
        record: Some(encode_record_with(objs[1].key, &objs[1].data, LZ4).unwrap()),
    };
    s.write_batch(obj_seq(&[rec], None)).unwrap();
    let got = segment_versions(dir.path());
    assert!(
        at(&got, 2) == count(1, 0) && at(&got, 3) == count(1, 1),
        "{got:?}"
    );
    for o in &objs {
        must_get(&s, o);
    }
}

#[test]
fn compaction_keeps_lz4_records_in_version_3_segments() {
    let dir = TempDir::new().unwrap();
    let objs = test_objects(40);
    let small = || Options::new().segment_size(8 << 10).sync(false);
    let w = Store::open_with(dir.path(), small().compression(LZ4)).unwrap();
    put_all(&w, &objs);
    w.close().unwrap();
    // The compacting handle never asked for lz4: what it copies decides.
    let s = Store::open_with(dir.path(), small()).unwrap();
    let live = |k: Key| k.as_bytes()[0].is_multiple_of(2);
    let stats = s.compact(live, CompactOpts::default()).unwrap();
    assert!(
        stats.records_copied > 0,
        "the pass copied nothing: {stats:?}"
    );
    let got = segment_versions(dir.path()); // panics on an lz4 record in a version-2 segment
    assert!(
        at(&got, 3).lz4 > 0,
        "no lz4 record survived the pass: {got:?}"
    );
    for o in objs.iter().filter(|o| live(o.key)) {
        must_get(&s, o);
    }
    s.verify(|| false).unwrap();
}

#[test]
fn empty_version_2_active_is_let_go_not_rewritten() {
    let lz4_on = Arc::new(AtomicBool::new(false));
    let dir = TempDir::new().unwrap();
    let s = Store::open_with(dir.path(), switchable(&lz4_on)).unwrap();
    // Own an active segment with nothing in it, as a store does after
    // adopting one that nobody wrote to.
    let empty = {
        let mut ap = s.append_lock();
        s.ensure_active(&mut ap).unwrap();
        ap.active.as_ref().unwrap().seg.path.clone()
    };
    let before = fs::read(&empty).unwrap();
    assert_eq!(before, b"AMBERSG\x02");

    lz4_on.store(true, Ordering::SeqCst);
    let o = blob_obj(&compressible(4096));
    s.put(o.key, &o.data).unwrap();
    // A running older process may already have read that header: the
    // segment is left as it is, for a writer that can use it.
    assert_eq!(
        fs::read(&empty).unwrap(),
        before,
        "the empty version-2 segment was rewritten"
    );
    let got = segment_versions(dir.path());
    assert!(
        at(&got, 2) == count(1, 0) && at(&got, 3) == count(1, 1),
        "{got:?}"
    );
    must_get(&s, &o);
}

#[test]
fn adoption_respects_the_segment_version() {
    let objs = distinct(2);
    let write_one = |dir: &Path, c: Compression, o: &Object| {
        let s = Store::open_with(dir, Options::new().sync(false).compression(c)).unwrap();
        s.put(o.key, &o.data).unwrap();
        s.close().unwrap();
    };

    // An lz4 handle leaves a version-2 segment alone.
    let dir = TempDir::new().unwrap();
    write_one(dir.path(), ZSTD, &objs[0]);
    let orphan = active_files(dir.path()).remove(0);
    write_one(dir.path(), LZ4, &objs[1]);
    assert_eq!(segment_records(&orphan), (2, vec![CODEC_ZSTD]));
    let got = segment_versions(dir.path());
    assert!(
        at(&got, 2) == count(1, 0) && at(&got, 3) == count(1, 1),
        "{got:?}"
    );

    // A zstd handle adopts a version-3 segment and goes on filling it.
    let dir = TempDir::new().unwrap();
    write_one(dir.path(), LZ4, &objs[0]);
    let orphan = active_files(dir.path()).remove(0);
    write_one(dir.path(), ZSTD, &objs[1]);
    assert_eq!(segment_records(&orphan), (3, vec![CODEC_LZ4, CODEC_ZSTD]));
    assert_eq!(
        segment_versions(dir.path()).len(),
        1,
        "only the adopted segment"
    );
}

#[test]
fn repair_raises_the_version_only_for_an_lz4_replacement() {
    for (lz4, want_version, want_codec) in [(false, 2, CODEC_ZSTD), (true, 3, CODEC_LZ4)] {
        let objs = vec![blob_obj(&compressible(8192))];
        let (dir, path, entries) = write_sealed_file(&objs); // one zstd record, version 2
        let mut bytes = fs::read(&path).unwrap();
        bytes[entries[0].off as usize + REC_HEADER_SIZE] ^= 0x40;
        fs::write(&path, bytes).unwrap();

        let lz4_on = Arc::new(AtomicBool::new(lz4));
        let s = Store::open_with(dir.path(), switchable(&lz4_on)).unwrap();
        s.put_verified(objs[0].key, &objs[0].data).unwrap();
        // A repair keeps the segment's name.
        assert_eq!(
            segment_records(&path),
            (want_version, vec![want_codec]),
            "lz4 replacement: {lz4}"
        );
        must_get(&s, &objs[0]);
        s.verify(|| false).unwrap();
    }
}

#[test]
fn verify_rejects_an_lz4_record_in_a_version_2_segment() {
    // Built by hand: no writer of this release produces it.
    let o = blob_obj(&compressible(4096));
    let rec = encode_record_with(o.key, &o.data, LZ4).unwrap();
    for (version, corrupt) in [(2u8, true), (3, false)] {
        let mut body = MAGIC_HEADER.to_vec();
        body[7] = version;
        body.extend_from_slice(&rec);
        let entry = IndexEntry {
            k: o.key,
            off: MAGIC_HEADER.len() as u64,
            slen: (rec.len() - REC_HEADER_SIZE) as u32,
        };
        let footer = build_footer(body.len() as u64, &[entry]).unwrap();
        body.extend_from_slice(&footer);
        let dir = TempDir::new().unwrap();
        fs::write(dir.path().join("0000000000000001.seg"), &body).unwrap();

        let s = Store::open(dir.path()).unwrap_or_else(|e| panic!("version {version}: {e}"));
        must_get(&s, &o); // reading decodes by the record's own codec
        match s.verify(|| false) {
            Err(e) => assert!(corrupt && e.is_corrupt(), "version {version}: {e}"),
            Ok(()) => assert!(
                !corrupt,
                "version {version}: an lz4 record in a version-2 segment passed the scrub"
            ),
        }
    }
}

#[test]
fn open_refuses_a_version_beyond_the_ones_it_reads() {
    let dir = TempDir::new().unwrap();
    let s = Store::open_with(
        dir.path(),
        Options::new()
            .segment_size(4 << 10)
            .sync(false)
            .compression(LZ4),
    )
    .unwrap();
    put_all(&s, &test_objects(20));
    s.close().unwrap();
    let sealed = sealed_files(dir.path());
    assert!(!sealed.is_empty());
    let mut b = fs::read(&sealed[0]).unwrap();
    b[7] = 4;
    fs::write(&sealed[0], b).unwrap();
    let err = Store::open(dir.path()).expect_err("a version-4 segment must be refused");
    assert!(err.is_unsupported_version(), "{err}");
}

/// A handle has to leave its version-2 segment for an lz4 record and, in
/// doing so, adopts a version-3 segment another writer left behind — one
/// that already holds that very record, not yet known durable. The record
/// must not be appended to the segment a second time: a sealed segment with
/// a key twice fails the scrub (Go:
/// `TestLeavingForAVersion3SegmentChecksItForTheKey`).
#[test]
fn leaving_for_a_version_3_segment_checks_it_for_the_key() {
    let dir = TempDir::new().unwrap();
    let objs = distinct(2);
    let (k0, k1) = (&objs[0], &objs[1]);

    // W writes k1 as lz4 without syncing and stays open: its version-3
    // segment is taken, so H below has to make one of its own.
    let w = Store::open_with(dir.path(), Options::new().sync(false).compression(LZ4)).unwrap();
    w.put(k1.key, &k1.data).unwrap();

    let lz4_on = Arc::new(AtomicBool::new(false));
    let h = Store::open_with(dir.path(), switchable(&lz4_on).sync(true)).unwrap();
    h.put(k0.key, &k0.data).unwrap();
    w.close().unwrap();

    // H now puts k1 as lz4: it leaves its version-2 segment and adopts W's.
    lz4_on.store(true, Ordering::SeqCst);
    h.put(k1.key, &k1.data).unwrap();

    let records: usize = sealed_files(dir.path())
        .into_iter()
        .chain(active_files(dir.path()))
        .map(|p| segment_records(&p).1.len())
        .sum();
    assert_eq!(records, 2, "{:?}", segment_versions(dir.path()));
    // Sealed, the segment must pass the scrub.
    {
        let mut ap = h.append_lock();
        h.seal_active(&mut ap).unwrap();
    }
    h.verify(|| false).unwrap();
    must_get(&h, k0);
    must_get(&h, k1);
}

/// Two lz4 records in a version-3 segment; one is damaged and repaired with
/// a zstd replacement. The other lz4 record is still there, so the segment
/// must stay at version 3 (Go: `TestRepairKeepsTheVersionOfAVersion3Segment`).
#[test]
fn repair_keeps_the_version_of_a_version_3_segment() {
    let lz4_on = Arc::new(AtomicBool::new(true));
    let dir = TempDir::new().unwrap();
    let s = Store::open_with(dir.path(), switchable(&lz4_on)).unwrap();
    let objs = distinct(2);
    for o in &objs {
        s.put(o.key, &o.data).unwrap();
    }
    {
        let mut ap = s.append_lock();
        s.seal_active(&mut ap).unwrap();
    }
    let path = sealed_files(dir.path()).remove(0);
    assert_eq!(segment_records(&path), (3, vec![CODEC_LZ4, CODEC_LZ4]));
    // The first record's payload starts right behind the segment header and
    // the record header.
    let mut bytes = fs::read(&path).unwrap();
    bytes[MAGIC_HEADER.len() + REC_HEADER_SIZE] ^= 0x40;
    fs::write(&path, bytes).unwrap();

    lz4_on.store(false, Ordering::SeqCst);
    assert!(
        s.verify(|| false).is_err(),
        "the damage must be visible to the scrub"
    );
    for o in &objs {
        s.put_verified(o.key, &o.data).unwrap();
    }
    let (version, codecs) = segment_records(&path);
    assert_eq!(
        version, 3,
        "a repair must not lower the version: {codecs:?}"
    );
    assert_eq!(codecs.len(), 2);
    assert!(
        codecs.contains(&CODEC_ZSTD) && codecs.contains(&CODEC_LZ4),
        "{codecs:?}"
    );
    for o in &objs {
        must_get(&s, o);
    }
    s.verify(|| false).unwrap();
}
