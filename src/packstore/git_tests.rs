//! Git records and the segment gate: a git record is only ever in a version-4
//! segment (Go: `packstore/git_test.go`).

use std::collections::HashMap;
use std::fs;
use std::path::Path;

use tempfile::TempDir;

use super::compression_tests::{CODEC_LZ4, LZ4, codec_of, distinct, must_get, raw_store};
use super::footer::{IndexEntry, build_footer};
use super::gc_tests::compact_store;
use super::store_tests::{active_files, obj_seq, sealed_files};
use super::testutil::*;
use super::{CompactOpts, MAGIC_HEADER, Object, Options, Store, WriteOpts};
use crate::amberpack::{REC_HEADER_SIZE, encode_record, encode_record_with, parse_record};
use crate::gitobj::{self, Kind};
use crate::key::{GIT_SHA1_SIZE, Key, Type};

fn git_blob_obj(content: &[u8]) -> Object {
    let data = gitobj::encode(Kind::Blob, content);
    Object {
        key: gitobj::key_of(&data).unwrap(),
        data,
        record: None,
    }
}

/// n distinct git blobs of a few hundred bytes.
fn git_blob_objs(n: usize) -> Vec<Object> {
    (0..n)
        .map(|i| {
            let mut c = incompressible(600);
            c.push(i as u8);
            c.push((i >> 8) as u8);
            git_blob_obj(&c)
        })
        .collect()
}

/// A segment's version byte and how many git records it holds.
fn segment_git_records(path: &Path) -> (u8, usize) {
    let b = fs::read(path).unwrap();
    assert!(b.len() >= 8 && &b[..7] == b"AMBERSG", "{}", path.display());
    let mut git = 0;
    let mut off = 8;
    while off < b.len() && b[off] == 0x01 {
        let rec = parse_record(&b[off..]).unwrap();
        if rec.key.type_() == Type::GitSha1 {
            git += 1;
        }
        off += REC_HEADER_SIZE + rec.slen as usize;
    }
    (b[7], git)
}

/// Walks every segment in `dir` and checks the gate's rule: a git record is in a
/// version-4 segment and nowhere else. Returns, per version, the segments there are
/// and the git records they hold.
fn git_per_version(dir: &Path) -> HashMap<u8, (usize, usize)> {
    let mut out: HashMap<u8, (usize, usize)> = HashMap::new();
    for p in sealed_files(dir).into_iter().chain(active_files(dir)) {
        let (v, git) = segment_git_records(&p);
        assert!(
            git == 0 || v == 4,
            "{} is a version-{v} segment and holds {git} git records",
            p.display()
        );
        let c = out.entry(v).or_default();
        c.0 += 1;
        c.1 += git;
    }
    out
}

fn at(got: &HashMap<u8, (usize, usize)>, v: u8) -> (usize, usize) {
    got.get(&v).copied().unwrap_or_default()
}

fn seal(s: &Store) {
    let mut ap = s.append_lock();
    s.seal_active(&mut ap).unwrap();
}

#[test]
fn first_git_record_moves_the_handle_to_version_4() {
    let dir = TempDir::new().unwrap();
    let s = Store::open_with(dir.path(), Options::new().sync(false)).unwrap();
    let plain = distinct(2);
    let git = git_blob_objs(2);
    s.put(plain[0].key, &plain[0].data).unwrap();
    let got = git_per_version(dir.path());
    assert!(got.len() == 1 && at(&got, 2).0 == 1, "{got:?}");

    s.put(git[0].key, &git[0].data).unwrap();
    let got = git_per_version(dir.path());
    assert_eq!((at(&got, 2), at(&got, 4)), ((1, 0), (1, 1)), "{got:?}");
    assert_eq!(
        sealed_files(dir.path()).len(),
        1,
        "the version-2 one is left"
    );

    // A plain record goes on in the version-4 segment, and the handle's next
    // segment is at version 4 too: one early seal, not one per segment.
    s.put(plain[1].key, &plain[1].data).unwrap();
    seal(&s);
    s.put(git[1].key, &git[1].data).unwrap();
    let got = git_per_version(dir.path());
    assert_eq!((at(&got, 2), at(&got, 4)), ((1, 0), (2, 2)), "{got:?}");

    for o in plain.iter().chain(&git) {
        must_get(&s, o);
    }
    s.verify(|| false).unwrap();
}

/// lz4 needs version 3 and a git record version 4: a version-4 segment holds both,
/// and a version-3 one is left like a version-2 one.
#[test]
fn git_record_leaves_a_version_3_segment() {
    let dir = TempDir::new().unwrap();
    let s = Store::open_with(dir.path(), Options::new().sync(false).compression(LZ4)).unwrap();
    let plain = blob_obj(&compressible(4096));
    let git = git_blob_obj(&compressible(4096));
    s.put(plain.key, &plain.data).unwrap();
    s.put(git.key, &git.data).unwrap();
    let got = git_per_version(dir.path());
    assert_eq!((at(&got, 3), at(&got, 4)), ((1, 0), (1, 1)), "{got:?}");
    assert_eq!(codec_of(&s, git.key), CODEC_LZ4);
    // And lz4 records are at home in version 4.
    let mut d = compressible(4096);
    d.push(1);
    let later = blob_obj(&d);
    s.put(later.key, &later.data).unwrap();
    for o in [&plain, &git, &later] {
        must_get(&s, o);
    }
    seal(&s);
    s.verify(|| false).unwrap();
}

#[test]
fn git_records_through_every_write_path() {
    type Write = fn(&Store, &[Object]);
    let paths: [(&str, Write); 5] = [
        ("put", |s, objs| put_all(s, objs)),
        ("put_verified", |s, objs| {
            for o in objs {
                s.put_verified(o.key, &o.data).unwrap();
            }
        }),
        ("write_batch", |s, objs| {
            s.write_batch(obj_seq(objs, None)).unwrap();
        }),
        ("write_batch of encoded records", |s, objs| {
            let recs: Vec<Object> = objs
                .iter()
                .map(|o| Object {
                    key: o.key,
                    data: Vec::new(),
                    record: Some(encode_record_with(o.key, &o.data, ZSTD).unwrap()),
                })
                .collect();
            s.write_batch(obj_seq(&recs, None)).unwrap();
        }),
        ("write_parallel", |s, objs| {
            let (_, r) = s.write_parallel(
                obj_seq(objs, None),
                WriteOpts {
                    verify: true,
                    writers: 4,
                    ..WriteOpts::default()
                },
            );
            r.unwrap();
        }),
    ];
    for (name, write) in paths {
        let dir = TempDir::new().unwrap();
        let s =
            Store::open_with(dir.path(), Options::new().segment_size(4 << 10).sync(false)).unwrap();
        let objs = git_blob_objs(40);
        write(&s, &objs);
        let got = git_per_version(dir.path()); // fails on a git record below version 4
        assert_eq!(at(&got, 4).1, objs.len(), "{name}: {got:?}");
        for o in &objs {
            must_get(&s, o);
        }
    }
}

#[test]
fn compaction_keeps_git_records_in_version_4_segments() {
    let dir = TempDir::new().unwrap();
    let objs = git_blob_objs(40);
    let opts = || Options::new().segment_size(8 << 10).sync(false);
    let w = Store::open_with(dir.path(), opts()).unwrap();
    put_all(&w, &objs);
    w.close().unwrap();
    // The compacting handle has seen no git record: what it copies decides.
    let s = Store::open_with(dir.path(), opts()).unwrap();
    let live = |k: Key| k.0[0].is_multiple_of(2);
    let stats = s.compact(live, CompactOpts::default()).unwrap();
    assert!(stats.records_copied > 0, "{stats:?}");
    let got = git_per_version(dir.path()); // fails on a git record below version 4
    assert!(at(&got, 4).1 > 0, "{got:?}");
    for o in objs.iter().filter(|o| live(o.key)) {
        must_get(&s, o);
    }
    s.verify(|| false).unwrap();
}

/// Reopened without a sidecar the active segment is scanned; a release that does not
/// know the type takes a git record for a torn tail there. This one does not.
#[test]
fn git_records_survive_a_scan_of_the_active_segment() {
    let dir = TempDir::new().unwrap();
    let objs = git_blob_objs(10);
    let s = Store::open_with(dir.path(), Options::new().sync(false)).unwrap();
    put_all(&s, &objs);
    s.close().unwrap();
    let sidecars = sidecar_files(dir.path());
    assert!(!sidecars.is_empty());
    for p in sidecars {
        fs::remove_file(p).unwrap();
    }
    let r = Store::open_with(dir.path(), Options::new().sync(false)).unwrap();
    for o in &objs {
        must_get(&r, o);
    }
    // Writing is what makes the scan truncate.
    let extra = blob_obj(b"after the scan");
    r.put(extra.key, &extra.data).unwrap();
    for o in &objs {
        must_get(&r, o);
    }
    must_get(&r, &extra);
}

/// A hand-made sealed segment at `version` holding one raw record.
fn sealed_at(version: u8, o: &Object) -> (TempDir, std::path::PathBuf) {
    let rec = encode_record(o.key, &o.data).unwrap();
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
    let path = dir.path().join("0000000000000001.seg");
    fs::write(&path, &body).unwrap();
    (dir, path)
}

#[test]
fn verify_rejects_a_git_record_below_version_4() {
    // Built by hand: no writer of this release produces it.
    let o = git_blob_obj(b"hello\n");
    for (version, corrupt) in [(2u8, true), (3, true), (4, false)] {
        let (dir, _) = sealed_at(version, &o);
        let s = Store::open(dir.path()).unwrap();
        must_get(&s, &o);
        let r = s.verify(|| false);
        if corrupt {
            let err = r.expect_err("a git record below version 4 passed the scrub");
            assert!(err.is_corrupt(), "version {version}: {err}");
            assert!(
                err.to_string().contains(&format!(
                    "GitSHA1 key, codec 0 in a version-{version} segment"
                )),
                "{err}"
            );
        } else {
            r.unwrap();
        }
    }
}

/// A repair raises the segment for its replacement, as for lz4: a git record that
/// somehow sits lower cannot get a segment it may not be in.
#[test]
fn repair_raises_the_version_for_a_git_record() {
    let o = git_blob_obj(&incompressible(8192));
    let (dir, path) = sealed_at(3, &o);
    let s = Store::open_with(dir.path(), Options::new().sync(false)).unwrap();
    let mut bytes = fs::read(&path).unwrap();
    bytes[MAGIC_HEADER.len() + REC_HEADER_SIZE] ^= 0x40;
    fs::write(&path, bytes).unwrap();
    s.put_verified(o.key, &o.data).unwrap();
    assert_eq!(segment_git_records(&path), (4, 1));
    must_get(&s, &o);
}

#[test]
fn verify_object_holds_a_git_object_to_its_sha1() {
    use super::verify::verify_object;
    let good = git_blob_obj(b"hello\n");
    verify_object(good.key, &good.data).unwrap();
    let err = verify_object(good.key, b"blob 6\0hullo\n").unwrap_err();
    assert!(
        err.starts_with(&format!(
            "packstore: object verification failed: {}: gitobj: payload does not match its key: ",
            good.key
        )),
        "{err}"
    );
    let unframed = b"hello\n";
    let k = gitobj::key_of(unframed).unwrap();
    let err = verify_object(k, unframed).unwrap_err();
    assert!(err.contains("gitobj: malformed object"), "{err}");
    // A commit that does not parse is the walks' to refuse, if they ever read it.
    let junk = gitobj::encode(Kind::Commit, b"junk");
    let k = gitobj::key_of(&junk).unwrap();
    verify_object(k, &junk).unwrap();
}

#[test]
fn verify_object_refuses_a_collision() {
    use super::verify::verify_object;
    let hex = concat!(
        "255044462d312e330a25e2e3cfd30a0a0a312030206f626a0a3c3c2f57696474",
        "682032203020522f4865696768742033203020522f547970652034203020522f",
        "537562747970652035203020522f46696c7465722036203020522f436f6c6f72",
        "53706163652037203020522f4c656e6774682038203020522f42697473506572",
        "436f6d706f6e656e7420383e3e0a73747265616d0affd8fffe00245348412d31",
        "20697320646561642121212121852fec092339759c39b1a1c63c4c97e1fffe01",
        "7346dc9166b67e118f029ab621b2560ff9ca67cca8c7f85ba84c79030c2b3de2",
        "18f86db3a90901d5df45c14f26fedfb3dc38e96ac22fe7bd728f0e45bce046d2",
        "3c570feb141398bb552ef5a0a82be331fea48037b8b5d71f0e332edf93ac3500",
        "eb4ddc0decc1a864790c782c76215660dd309791d06bd0af3f98cda4bc4629b1",
    );
    let data: Vec<u8> = (0..hex.len() / 2)
        .map(|i| u8::from_str_radix(&hex[2 * i..2 * i + 2], 16).unwrap())
        .collect();
    let mut sum = [0u8; GIT_SHA1_SIZE];
    let oid = "f92d74e3874587aaf443d1db961d4e26dde13e9c";
    for (i, b) in sum.iter_mut().enumerate() {
        *b = u8::from_str_radix(&oid[2 * i..2 * i + 2], 16).unwrap();
    }
    let err = verify_object(Key::new_git_sha1(sum), &data).unwrap_err();
    assert!(
        err.contains("gitobj: SHA-1 collision attack detected"),
        "{err}"
    );
}

/// A forged object is bad input, not a damaged store: the same verdict class as Go's
/// `errors.Is(err, ErrVerify)` and `!errors.Is(err, ErrCorrupt)`, for a caller telling
/// the two apart. (Go: `putVerified`.)
#[test]
fn put_verified_calls_a_forged_object_bad_input_not_corruption() {
    let (_dir, s) = raw_store(Options::new());
    let o = git_blob_obj(b"hello\n");
    let err = s.put_verified(o.key, b"blob 6\0hullo\n").unwrap_err();
    assert!(err.is_verify(), "{err}");
    assert!(!err.is_corrupt(), "{err}");
    let plain = blob_obj(b"hello");
    let err = s.put_verified(plain.key, b"hullo").unwrap_err();
    assert!(err.is_verify(), "{err}");
    assert!(!err.is_corrupt(), "{err}");
}

// The promise of architecture/git-objects.md, at each place that makes it: a plain
// put verifies nothing, and each of these sees what it wrote.
fn wrong_git_payload(s: &Store) -> Object {
    let o = git_blob_obj(b"hello\n");
    s.put(o.key, b"blob 6\0hullo\n").unwrap();
    o
}

#[test]
fn compaction_refuses_a_wrong_git_payload() {
    let dir = TempDir::new().unwrap();
    let s = Store::open_with(dir.path(), Options::new().sync(false)).unwrap();
    let o = wrong_git_payload(&s);
    let dead = git_blob_obj(b"dead\n");
    s.put(dead.key, &dead.data).unwrap();
    let keep = o.key;
    let err = s
        .compact(move |k| k == keep, CompactOpts::default())
        .expect_err("a compaction copied a wrong payload");
    assert!(err.is_verify(), "{err}");
    assert!(s.has(o.key).unwrap());
}

#[test]
fn scrub_refuses_a_wrong_git_payload() {
    let dir = TempDir::new().unwrap();
    let s = Store::open_with(dir.path(), Options::new().sync(false)).unwrap();
    wrong_git_payload(&s);
    seal(&s);
    let err = s.verify(|| false).expect_err("a scrub passed it");
    assert!(err.is_verify(), "{err}");
}

#[test]
fn put_verified_replaces_a_wrong_git_payload() {
    let dir = TempDir::new().unwrap();
    let s = Store::open_with(dir.path(), Options::new().sync(false)).unwrap();
    let o = wrong_git_payload(&s);
    s.put_verified(o.key, &o.data).unwrap();
    must_get(&s, &o);
}

#[test]
fn write_parallel_refuses_a_forged_git_object() {
    let o = git_blob_obj(b"hello\n");
    let forged = b"blob 6\0hullo\n".to_vec();
    let objs = [
        Object {
            key: o.key,
            data: forged.clone(),
            record: None,
        },
        Object {
            key: o.key,
            data: Vec::new(),
            record: Some(encode_record(o.key, &forged).unwrap()),
        },
    ];
    for obj in objs {
        let (_dir, s) = raw_store(Options::new());
        let (_, r) = s.write_parallel(
            obj_seq(std::slice::from_ref(&obj), None),
            WriteOpts {
                verify: true,
                writers: 2,
                ..WriteOpts::default()
            },
        );
        let err = r.expect_err("a forged git object got in");
        assert!(err.is_verify(), "{err}");
        assert!(!s.has(o.key).unwrap());
    }
}

#[test]
fn repair_of_a_git_record_keeps_version_4() {
    let (dir, s) = raw_store(Options::new().compression(ZSTD));
    let objs = git_blob_objs(2);
    for o in &objs {
        s.put(o.key, &o.data).unwrap();
    }
    seal(&s);
    let path = sealed_files(dir.path()).remove(0);
    let mut bytes = fs::read(&path).unwrap();
    bytes[MAGIC_HEADER.len() + REC_HEADER_SIZE] ^= 0x40;
    fs::write(&path, bytes).unwrap();
    s.put_verified(objs[0].key, &objs[0].data).unwrap();
    assert_eq!(segment_git_records(&path), (4, 2));
    for o in &objs {
        must_get(&s, o);
    }
    s.verify(|| false).unwrap();
}

#[test]
fn compression_callback_is_asked_for_git_objects() {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    let asked = Arc::new(AtomicUsize::new(0));
    let seen = Arc::clone(&asked);
    let (_dir, s) = raw_store(Options::new().compression_for(move |k, _, def| {
        if k.type_() == Type::GitSha1 {
            seen.fetch_add(1, Ordering::SeqCst);
        }
        def
    }));
    let o = git_blob_obj(b"hello\n");
    s.put(o.key, &o.data).unwrap();
    assert_eq!(asked.load(Ordering::SeqCst), 1);
}

/// A store's dedup check answers for the SHA-1, so what is verified on the way in is
/// all that stands between a key and the wrong bytes.
#[test]
fn put_verified_refuses_the_wrong_payload_under_a_git_key() {
    let (_dir, s) = raw_store(Options::new());
    let o = git_blob_obj(b"hello\n");
    s.put_verified(o.key, b"blob 6\0hullo\n")
        .expect_err("the wrong payload was stored");
    assert!(!s.has(o.key).unwrap());
}

#[test]
fn mark_set_mark_walked() {
    let (_dir, s, objs) = compact_store(); // sealed segments and an active one
    let git = git_blob_obj(b"in the active segment");
    s.put(git.key, &git.data).unwrap();
    let mut all = objs.clone();
    all.push(git);
    let mut m = s.new_mark_set();
    for (i, o) in all.iter().enumerate() {
        assert!(!m.contains(o.key), "object {i} marked before mark");
        // Marking does not walk: that is the point of a second bit.
        assert_eq!(m.mark(o.key), (true, true), "object {i}");
        assert!(m.mark_walked(o.key), "first mark_walked of object {i}");
        assert!(!m.mark_walked(o.key), "second mark_walked of object {i}");
        assert_eq!(m.mark(o.key), (false, true), "object {i} after mark_walked");
    }
    assert_eq!(m.marked(), all.len(), "walking is no second mark");
    assert!(!m.mark_walked(blob_obj(b"never stored").key));
    // Walking is no mark, on either kind of segment.
    let mut fresh = s.new_mark_set();
    for (i, o) in all.iter().enumerate() {
        assert!(fresh.mark_walked(o.key), "mark_walked of object {i}");
        assert!(!fresh.contains(o.key), "mark_walked marked object {i}");
    }
    assert_eq!(fresh.marked(), 0, "after walking alone");
}
