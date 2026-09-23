//! Ported Go tests of the sidecar index and of recovery from it
//! (`sidecar_test.go`, `recover_sidecar_test.go`).

use std::fs::{self, File};
use std::io;
use std::sync::atomic::{AtomicU64, Ordering};

use tempfile::TempDir;

use crate::amberpack::{REC_HEADER_SIZE, parse_record};
use crate::key::{Key, Type};

use super::recover::ActiveLoc;
use super::recover_sidecar::{DataAt, recover_from};
use super::sidecar::{
    SIDECAR_ENTRY, SIDECAR_MAGIC, SIDECAR_REC_SIZE, SIDECAR_SUFFIX, SidecarRec, SidecarWriter,
    read_sidecar,
};
use super::store_tests::obj_seq;
use super::testutil::*;
use super::{MAGIC_HEADER, MAGIC_TRAILER, Options, Store};

fn sc_key(i: usize) -> Key {
    let data = format!("sidecar-object-{i}");
    Key::new(Type::Blob, data.len() as u64, data.as_bytes())
}

fn sc_entry(i: usize) -> SidecarRec {
    SidecarRec::entry(
        sc_key(i),
        ActiveLoc {
            off: 8 + 100 * i as u64,
            flags: (i % 2) as u8,
            ulen: 50 + i as u32,
            slen: 40 + i as u32,
        },
    )
}

/// Recomputes the CRC of a hand-damaged record, so that a test reaches the
/// checks behind it (Go: `resealSidecarRec`).
fn reseal(mut b: [u8; SIDECAR_REC_SIZE]) -> [u8; SIDECAR_REC_SIZE] {
    let crc = crc32c::crc32c(&b[..52]);
    b[52..56].copy_from_slice(&crc.to_be_bytes());
    b
}

fn entries_listed(recs: &[SidecarRec]) -> usize {
    recs.iter().filter(|r| r.kind == SIDECAR_ENTRY).count()
}

#[test]
fn sidecar_record_round_trip() {
    for want in [sc_entry(1), sc_entry(2), SidecarRec::synced(1 << 40)] {
        let enc = want.encode();
        assert_eq!(SidecarRec::decode(&enc), Some(want));
        for bit in 0..enc.len() * 8 {
            let mut flipped = enc;
            flipped[bit / 8] ^= 1 << (bit % 8);
            assert!(
                SidecarRec::decode(&flipped).is_none(),
                "kind {:#x}: a flip of bit {bit} went unnoticed",
                want.kind
            );
        }
    }
    let e = sc_entry(3);
    assert_eq!(
        e.loc(),
        ActiveLoc {
            off: 308,
            flags: 1,
            ulen: 53,
            slen: 43
        }
    );
    assert_eq!(e.end(), 308 + 46 + 43);
}

#[test]
fn decode_rejects_malformed_records() {
    let mut unknown = sc_entry(1).encode();
    unknown[0] = 0x7f;
    let mut reserved = sc_entry(1).encode();
    reserved[50] = 1;
    let mut synced = SidecarRec::synced(99).encode();
    synced[5] = 1; // a key byte: a synced record carries none
    for (name, b) in [
        ("unknown kind", unknown),
        ("reserved bytes set", reserved),
        ("synced record with a key", synced),
    ] {
        assert!(SidecarRec::decode(&reseal(b)).is_none(), "{name}: accepted");
    }
    assert!(
        SidecarRec::decode(&[0u8; SIDECAR_REC_SIZE - 1]).is_none(),
        "a short record was accepted"
    );
}

#[test]
fn read_sidecar_stops_at_first_bad_record() {
    let recs = [sc_entry(1), SidecarRec::synced(1000), sc_entry(2)];
    let img = sidecar_image(&recs);
    for cut in 0..=img.len() {
        let (got, valid) = read_sidecar(&img[..cut], true);
        let (mut whole, mut want_valid) = (0, 0);
        if cut >= SIDECAR_MAGIC.len() {
            whole = (cut - SIDECAR_MAGIC.len()) / SIDECAR_REC_SIZE;
            want_valid = SIDECAR_MAGIC.len() + whole * SIDECAR_REC_SIZE;
        }
        assert_eq!((got.len(), valid), (whole, want_valid), "cut at {cut}");
        assert_eq!(got[..], recs[..whole], "cut at {cut}");
    }
    // A damaged middle record hides everything after it.
    let mut damaged = img.clone();
    damaged[SIDECAR_MAGIC.len() + SIDECAR_REC_SIZE + 10] ^= 0xff;
    let (got, valid) = read_sidecar(&damaged, true);
    assert_eq!(
        (got.len(), valid),
        (1, SIDECAR_MAGIC.len() + SIDECAR_REC_SIZE)
    );
    // Continuing from a record boundary needs no magic.
    let (rest, n) = read_sidecar(&img[SIDECAR_MAGIC.len() + SIDECAR_REC_SIZE..], false);
    assert_eq!((rest.len(), n), (2, 2 * SIDECAR_REC_SIZE));
    assert_eq!(rest[1], recs[2]);
}

#[test]
fn read_sidecar_rejects_bad_magic() {
    let mut img = sidecar_image(&[sc_entry(1)]);
    img[0] ^= 0xff;
    let (got, valid) = read_sidecar(&img, true);
    assert_eq!((got.len(), valid), (0, 0));
}

#[test]
fn sidecar_writer_appends_and_resumes() {
    let dir = TempDir::new().unwrap();
    let path = dir
        .path()
        .join(format!("0000000000000001.seg.active{SIDECAR_SUFFIX}"));
    let (e1, e2, e3) = (sc_entry(1), sc_entry(2), sc_entry(3));
    let mut w = SidecarWriter::create(&path).unwrap();
    w.entry(e1.k, e1.loc());
    w.synced(500);
    w.entry(e2.k, e2.loc());
    drop(w);
    // Resume after the second record: the third is cut off and replaced.
    let mut w =
        SidecarWriter::open_at(&path, (SIDECAR_MAGIC.len() + 2 * SIDECAR_REC_SIZE) as u64).unwrap();
    w.entry(e3.k, e3.loc());
    drop(w);
    let (got, _) = read_sidecar_file(&path);
    assert_eq!(got, [e1, SidecarRec::synced(500), e3]);
    // A resume point inside the magic starts the file over.
    drop(SidecarWriter::open_at(&path, 3).unwrap());
    let (got, valid) = read_sidecar_file(&path);
    assert_eq!((got.len(), valid), (0, SIDECAR_MAGIC.len()));
}

#[test]
fn sidecar_writer_stops_after_a_failure() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join(format!("x{SIDECAR_SUFFIX}"));
    drop(SidecarWriter::create(&path).unwrap());
    // Opened for reading only: the next write fails.
    let mut w = SidecarWriter::over(File::open(&path).unwrap(), SIDECAR_MAGIC.len() as u64);
    let e = sc_entry(1);
    w.entry(e.k, e.loc());
    let (off, broken) = w.state();
    assert!(broken, "a failed write did not mark the writer broken");
    w.synced(10); // must be a no-op: a later record may never follow a hole
    assert_eq!(w.state().0, off, "a broken writer kept writing");
}

// Entries covered by a synced record are trusted: opening does not read their
// records back. The old full scan would have stopped at the damage and
// truncated; reporting damage in the body is scrub's job.
#[test]
fn open_trusts_synced_entries() {
    let dir = TempDir::new().unwrap();
    let s = Store::open(dir.path()).unwrap();
    let objs = test_objects(5);
    put_all(&s, &objs);
    s.close().unwrap();
    let data = only_active(dir.path());
    let before = fs::metadata(&data).unwrap().len();
    // The first payload byte of the first record.
    flip_byte(&data, (MAGIC_HEADER.len() + REC_HEADER_SIZE) as u64);

    let s2 = Store::open(dir.path()).unwrap();
    for o in &objs {
        assert!(
            s2.has(o.key).unwrap(),
            "has({}) after a reopen from the sidecar",
            o.key
        );
    }
    assert_eq!(
        fs::metadata(&data).unwrap().len(),
        before,
        "trusted records were re-read and truncated"
    );
}

/// Counts the bytes read through it (Go: `countingReaderAt`).
struct Counting<'a> {
    data: &'a [u8],
    n: AtomicU64,
}

impl DataAt for Counting<'_> {
    fn read_exact_at(&self, buf: &mut [u8], off: u64) -> io::Result<()> {
        self.data.read_exact_at(buf, off)?;
        self.n.fetch_add(buf.len() as u64, Ordering::Relaxed);
        Ok(())
    }
}

#[test]
fn recovery_reads_only_the_untrusted_tail() {
    let objs = test_objects(6);
    let (body, spans) = build_body(&objs);
    let entries: Vec<SidecarRec> = spans
        .iter()
        .map(|sp| {
            let rec = parse_record(&body[sp.off as usize..]).unwrap();
            SidecarRec::entry(
                sp.k,
                ActiveLoc {
                    off: sp.off,
                    flags: rec.flags,
                    ulen: rec.ulen,
                    slen: rec.slen,
                },
            )
        })
        .collect();
    // The header check and the crashed-seal probe.
    let probe = (MAGIC_HEADER.len() + MAGIC_TRAILER.len()) as u64;

    // Everything known durable: no record is read.
    let mut all = entries.clone();
    all.push(SidecarRec::synced(body.len() as u64));
    let all = sidecar_image(&all);
    let cr = Counting {
        data: &body,
        n: AtomicU64::new(0),
    };
    let res = recover_from(&cr, body.len() as i64, &all)
        .unwrap()
        .expect("the sidecar is usable");
    assert_eq!(
        (
            res.index.len(),
            res.data_end,
            res.sidecar_end,
            res.missing.len()
        ),
        (objs.len(), body.len() as i64, all.len() as i64, 0)
    );
    let read = cr.n.load(Ordering::Relaxed);
    assert!(
        read <= probe,
        "recovery read {read} data bytes with everything synced, want at most {probe}"
    );

    // Only the first two records known durable: exactly the rest is read.
    let partial = sidecar_image(&[
        entries[0],
        entries[1],
        SidecarRec::synced(spans[2].off),
        entries[2],
        entries[3],
        entries[4],
        entries[5],
    ]);
    let cr = Counting {
        data: &body,
        n: AtomicU64::new(0),
    };
    let res = recover_from(&cr, body.len() as i64, &partial)
        .unwrap()
        .expect("the sidecar is usable");
    assert_eq!(res.index.len(), objs.len());
    assert_eq!(
        cr.n.load(Ordering::Relaxed),
        probe + body.len() as u64 - spans[2].off,
        "the unsynced records and nothing else"
    );
}

// A writer killed after its data fsync returned but before the entry reached
// the sidecar leaves an acknowledged record the sidecar does not list.
#[test]
fn acknowledged_record_missing_from_index_is_found() {
    let dir = TempDir::new().unwrap();
    let s = Store::open(dir.path()).unwrap();
    let objs = test_objects(3);
    put_all(&s, &objs);
    s.close().unwrap();
    let idx = sidecar_of(&only_active(dir.path()));
    let (recs, valid) = read_sidecar_file(&idx);
    // put wrote entry+synced per object, close one more synced: drop the last
    // object's entry and everything after it.
    assert_eq!(recs.len(), 7, "sidecar records");
    File::options()
        .write(true)
        .open(&idx)
        .unwrap()
        .set_len((valid - 3 * SIDECAR_REC_SIZE) as u64)
        .unwrap();

    let s2 = Store::open(dir.path()).unwrap();
    want_objects(&s2, &objs); // found by a store that only reads, which repairs nothing
    // The next writer takes the segment and brings its sidecar in line.
    let extra = &test_objects(4)[3..];
    put_all(&s2, extra);
    s2.close().unwrap();
    let (recs, _) = read_sidecar_file(&idx);
    assert_eq!(entries_listed(&recs), objs.len() + extra.len());
}

/// Copies an open store's files, as a crash would leave them (Go:
/// `crashedCopy`).
fn crashed_copy(from: &std::path::Path) -> TempDir {
    let to = TempDir::new().unwrap();
    let data = only_active(from);
    for path in [data.clone(), sidecar_of(&data)] {
        fs::copy(&path, to.path().join(path.file_name().unwrap())).unwrap();
    }
    to
}

#[test]
fn unsynced_entries_are_verified() {
    let nosync = Options::new().sync(false);
    let src = TempDir::new().unwrap();
    let s = Store::open_with(src.path(), nosync).unwrap();
    let objs = test_objects(5);
    s.write_batch(obj_seq(&objs, None)).unwrap();
    let dir = crashed_copy(src.path()); // no fsync ever ran: nothing is known durable
    let data = only_active(dir.path());
    // The last record's last payload byte.
    flip_byte(&data, fs::metadata(&data).unwrap().len() - 1);

    let s2 = Store::open_with(dir.path(), nosync).unwrap();
    want_objects(&s2, &objs[..4]);
    assert!(
        !s2.has(objs[4].key).unwrap(),
        "a record that fails its CRC was indexed from its sidecar entry"
    );

    // Stale entries must never resolve into records appended later.
    let more = &test_objects(9)[5..];
    put_all(&s2, more);
    s2.close().unwrap();
    for _ in 0..2 {
        let s3 = Store::open_with(dir.path(), nosync).unwrap();
        want_objects(&s3, &objs[..4]);
        want_objects(&s3, more);
        assert!(
            !s3.has(objs[4].key).unwrap(),
            "the dropped record came back"
        );
        s3.close().unwrap();
    }
}

#[test]
fn sidecar_problems_fall_back_to_a_full_scan() {
    type Damage = fn(&std::path::Path, u64);
    let damages: [(&str, Damage); 3] = [
        ("missing", |idx, _| fs::remove_file(idx).unwrap()),
        ("garbage", |idx, _| {
            fs::write(idx, b"junk".repeat(100)).unwrap()
        }),
        (
            "claims more durable data than the file holds",
            |idx, data_len| {
                fs::write(idx, sidecar_image(&[SidecarRec::synced(data_len + 1)])).unwrap()
            },
        ),
    ];
    for (name, damage) in damages {
        let dir = TempDir::new().unwrap();
        let s = Store::open(dir.path()).unwrap();
        let objs = test_objects(4);
        put_all(&s, &objs);
        s.close().unwrap();
        let data = only_active(dir.path());
        damage(&sidecar_of(&data), fs::metadata(&data).unwrap().len());

        let s2 = Store::open(dir.path()).unwrap();
        want_objects(&s2, &objs);
        // The writer that takes the segment rebuilds its sidecar.
        let extra = &test_objects(5)[4..];
        put_all(&s2, extra);
        s2.close().unwrap();
        let (recs, _) = read_sidecar_file(&sidecar_of(&data));
        assert_eq!(entries_listed(&recs), objs.len() + extra.len(), "{name}");
    }
}

#[test]
fn seal_removes_sidecar() {
    let dir = TempDir::new().unwrap();
    // Every record seals its segment.
    let s = Store::open_with(dir.path(), Options::new().segment_size(1)).unwrap();
    put_all(&s, &test_objects(3));
    assert_eq!(sidecar_files(dir.path()), Vec::<std::path::PathBuf>::new());
}

#[test]
fn crashed_seal_leaves_no_sidecar() {
    let dir = TempDir::new().unwrap();
    let s = Store::open(dir.path()).unwrap();
    let objs = test_objects(6);
    put_all(&s, &objs);
    s.close().unwrap();
    // The footer reached the file, the rename did not happen.
    let data = only_active(dir.path());
    crash_seal(dir.path());

    let s2 = Store::open(dir.path()).unwrap();
    want_objects(&s2, &objs);
    put_all(&s2, &test_objects(7)[6..]); // the write that takes the segment and finishes its seal
    assert!(
        !sidecar_of(&data).exists(),
        "the completed seal left its sidecar"
    );
    assert_eq!(
        sidecar_files(dir.path()).len(),
        1,
        "want only the new active segment's sidecar"
    );
}

#[test]
fn orphan_sidecar_is_removed() {
    let dir = TempDir::new().unwrap();
    let orphan = dir
        .path()
        .join(format!("00000000000000aa.seg.active{SIDECAR_SUFFIX}"));
    fs::write(&orphan, sidecar_image(&[])).unwrap();
    let _s = Store::open(dir.path()).unwrap();
    assert_eq!(
        sidecar_files(dir.path()),
        Vec::<std::path::PathBuf>::new(),
        "an index without its segment survived the open"
    );
}

#[test]
fn wipe_removes_sidecar() {
    let dir = TempDir::new().unwrap();
    let s = Store::open(dir.path()).unwrap();
    put_all(&s, &test_objects(1));
    assert_eq!(
        sidecar_files(dir.path()).len(),
        1,
        "a written active segment has no sidecar"
    );
    s.wipe().unwrap();
    assert_eq!(sidecar_files(dir.path()), Vec::<std::path::PathBuf>::new());
}
