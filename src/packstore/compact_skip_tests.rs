//! A bad live record keeps its pack, and the pass goes on (Go:
//! `packstore/compact_skip_test.go`).

use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::Path;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tempfile::TempDir;

use super::compression_tests::{distinct, must_get};
use super::footer::{IndexEntry, build_footer};
use super::git_tests::git_blob_obj;
use super::repair::{indexed_record_valid, valid_record};
use super::sidecar::SIDECAR_SYNCED;
use super::store_tests::active_files;
use super::testutil::*;
use super::view::{Lookup, segment_name};
use super::{
    ACTIVE_SUFFIX, CompactOpts, Error, MAGIC_HEADER, Object, Options, SEALED_SUFFIX, Store,
    VERSION_BASE, header_at, unpoison,
};
use crate::amberpack::{REC_HEADER_SIZE, encode_record};
use crate::key::{Key, Type};

/// Writes a sealed segment of raw records and returns where each record's payload
/// starts, to damage it by.
fn sealed_file(dir: &Path, id: u64, objs: &[&Object]) -> HashMap<Key, u64> {
    let mut body = header_at(VERSION_BASE).to_vec();
    let mut payload = HashMap::new();
    let mut entries = Vec::new();
    for o in objs {
        let rec = encode_record(o.key, &o.data).unwrap();
        entries.push(IndexEntry {
            k: o.key,
            off: body.len() as u64,
            slen: o.data.len() as u32,
        });
        payload.insert(o.key, (body.len() + REC_HEADER_SIZE) as u64);
        body.extend_from_slice(&rec);
    }
    let footer = build_footer(body.len() as u64, &entries).unwrap();
    body.extend_from_slice(&footer);
    fs::write(dir.join(segment_name(id, SEALED_SUFFIX)), &body).unwrap();
    payload
}

fn seg_exists(dir: &Path, id: u64) -> bool {
    dir.join(segment_name(id, SEALED_SUFFIX)).exists()
}

fn damage(dir: &Path, id: u64, off: u64) {
    flip_byte(&dir.join(segment_name(id, SEALED_SUFFIX)), off);
}

fn live_of(objs: &[&Object]) -> impl Fn(Key) -> bool + Sync + use<> {
    let set: HashSet<Key> = objs.iter().map(|o| o.key).collect();
    move |k| set.contains(&k)
}

fn open(dir: &Path) -> Store {
    Store::open_with(dir, Options::new().sync(false)).unwrap()
}

fn verified() -> CompactOpts {
    CompactOpts {
        mark_verified: true,
        ..CompactOpts::default()
    }
}

fn skip_stats(err: &Error) -> super::CompactStats {
    err.compact_stats()
        .unwrap_or_else(|| panic!("not a skip: {err}"))
        .clone()
}

/// Three segments, each one live and one dead record, the first's live one damaged.
fn three_victims() -> (TempDir, Vec<Object>, Vec<Object>) {
    let dir = TempDir::new().unwrap();
    let objs = distinct(6);
    for i in 0..3 {
        let payload = sealed_file(dir.path(), i as u64 + 1, &[&objs[i], &objs[3 + i]]);
        if i == 0 {
            damage(dir.path(), 1, payload[&objs[0].key]);
        }
    }
    (dir, objs[..3].to_vec(), objs[3..].to_vec())
}

fn refs(objs: &[Object]) -> Vec<&Object> {
    objs.iter().collect()
}

/// #29. One live record that fails verification keeps its segment, and the rest of the
/// pass goes on: the other victims are copied and removed.
#[test]
fn compact_leaves_a_bad_victim_and_reaps_the_rest() {
    let (dir, live, dead) = three_victims();
    let s = open(dir.path());
    let size = |id| {
        fs::metadata(dir.path().join(segment_name(id, SEALED_SUFFIX)))
            .unwrap()
            .len()
    };
    let freed = size(2) + size(3);

    let err = s
        .compact(live_of(&refs(&live)), verified())
        .expect_err("a bad record passed");
    assert!(err.is_corrupt(), "{err}");
    let stats = skip_stats(&err);
    assert_eq!(stats.skipped.len(), 1);
    let sk = &stats.skipped[0];
    assert_eq!(
        (sk.id, sk.key, sk.offset),
        (1, live[0].key, MAGIC_HEADER.len() as u64)
    );
    assert!(sk.path.ends_with(segment_name(1, SEALED_SUFFIX)));

    // What the stats say is what happened.
    assert_eq!(stats.victims, [2, 3]);
    assert_eq!(stats.segments_compacted, 2);
    assert_eq!(stats.bytes_freed, freed);
    assert!(seg_exists(dir.path(), 1));
    assert!(!seg_exists(dir.path(), 2) && !seg_exists(dir.path(), 3));
    for o in &live[1..] {
        must_get(&s, o);
    }
    for o in &dead[1..] {
        assert!(!s.has(o.key).unwrap(), "a dead object of a removed victim");
    }
    // The kept segment keeps what it held, its dead record too.
    assert!(s.has(dead[0].key).unwrap());
}

/// It has to say what to do, and name the object: a CRC failure says nothing else
/// about which one it was.
#[test]
fn compact_skip_error_names_the_key_and_what_to_do() {
    let (dir, live, _) = three_victims();
    let s = open(dir.path());
    let err = s
        .compact(live_of(&refs(&live)), verified())
        .unwrap_err()
        .to_string();
    for want in [
        "packstore: compaction left 1 segment in place (repair the record with PutVerified, or drop the references that reach it): ",
        live[0].key.to_string().as_str(),
        segment_name(1, SEALED_SUFFIX).as_str(),
        "record CRC mismatch",
    ] {
        assert!(err.contains(want), "no {want:?} in: {err}");
    }
}

/// The trap in carrying on: a key with a bad copy in one victim and a good copy in
/// another. The second victim may be removed only with the good copy kept, whichever
/// the pass meets first.
#[test]
fn compact_keeps_the_good_copy_of_a_key_whose_other_copy_is_bad() {
    for bad_in in [1u64, 2] {
        let dir = TempDir::new().unwrap();
        let objs = distinct(3);
        let k = &objs[0];
        let payload = sealed_file(dir.path(), 1, &[k, &objs[1]]);
        sealed_file(dir.path(), 2, &[k, &objs[2]]);
        damage(dir.path(), bad_in, payload[&k.key]);

        let s = Store::open(dir.path()).unwrap();
        // Selection asks once per segment, then the walk once more each. Before the
        // walk asks about the second copy, give the verifiers time to have found the
        // first one bad: that is when a pass could wrongly take the key as dealt with.
        let asked = Arc::new(AtomicU32::new(0));
        let want = k.key;
        let live = move |q: Key| {
            if q != want {
                return false;
            }
            if asked.fetch_add(1, Ordering::SeqCst) + 1 == 4 {
                std::thread::sleep(Duration::from_millis(200));
            }
            true
        };
        if let Err(e) = s.compact(live, verified()) {
            assert!(e.is_corrupt(), "bad copy in {bad_in}: {e}");
        }
        assert!(
            !seg_exists(dir.path(), 3 - bad_in),
            "bad copy in {bad_in}: the victim with the good copy is still there"
        );
        assert_eq!(s.get(k.key).unwrap(), k.data, "bad copy in {bad_in}");
        s.close().unwrap();
        // And after a restart.
        let r = Store::open(dir.path()).unwrap();
        assert_eq!(
            r.get(k.key).unwrap(),
            k.data,
            "bad copy in {bad_in}, reopened"
        );
    }
}

/// A kept segment is a victim again on the next pass. What it already gave up stays
/// where it went, so the store does not grow by a copy a cycle.
#[test]
fn compact_does_not_grow_around_a_bad_victim() {
    let dir = TempDir::new().unwrap();
    let objs = distinct(5);
    let payload = sealed_file(dir.path(), 1, &refs(&objs));
    damage(dir.path(), 1, payload[&objs[0].key]);
    let s = open(dir.path());
    let live_set = refs(&objs[..4]); // objs[4] is dead, so the segment is a victim
    let mut copied = 0;
    for pass in 0..5 {
        let err = s.compact(live_of(&live_set), verified()).unwrap_err();
        let stats = skip_stats(&err);
        copied += stats.records_copied;
        assert!(stats.victims.is_empty(), "pass {pass}");
        assert!(seg_exists(dir.path(), 1), "pass {pass}");
    }
    assert!(copied <= 3, "{copied} records copied over 5 passes");
}

/// What clears it: a repair, or the reference going.
#[test]
fn compact_reaps_a_repaired_victim() {
    let (dir, live, _) = three_victims();
    let s = open(dir.path());
    s.compact(live_of(&refs(&live)), verified()).unwrap_err();
    s.put_verified(live[0].key, &live[0].data).unwrap();
    let stats = s.compact(live_of(&refs(&live)), verified()).unwrap();
    assert!(stats.skipped.is_empty());
    assert!(stats.victims.contains(&1));
    assert!(!seg_exists(dir.path(), 1));
    must_get(&s, &live[0]);
}

#[test]
fn compact_reaps_a_victim_once_its_bad_record_is_dead() {
    let (dir, live, _) = three_victims();
    let s = open(dir.path());
    s.compact(live_of(&refs(&live)), verified()).unwrap_err();
    let stats = s.compact(live_of(&refs(&live[1..])), verified()).unwrap();
    assert!(stats.skipped.is_empty());
    assert!(!seg_exists(dir.path(), 1));
}

#[test]
fn compact_every_victim_bad() {
    let dir = TempDir::new().unwrap();
    let objs = distinct(4);
    for i in 0..2 {
        let payload = sealed_file(dir.path(), i as u64 + 1, &[&objs[i], &objs[2 + i]]);
        damage(dir.path(), i as u64 + 1, payload[&objs[i].key]);
    }
    let s = open(dir.path());
    let err = s
        .compact(live_of(&refs(&objs[..2])), verified())
        .unwrap_err();
    let stats = skip_stats(&err);
    let ids: Vec<u64> = stats.skipped.iter().map(|sk| sk.id).collect();
    assert_eq!(ids, [1, 2], "ascending");
    assert!(stats.victims.is_empty());
    assert_eq!(stats.bytes_freed, 0);
    assert!(seg_exists(dir.path(), 1) && seg_exists(dir.path(), 2));
}

/// A bad index entry is the same kind of trouble: that segment stays.
#[test]
fn compact_leaves_a_victim_with_a_bad_index_entry() {
    let dir = TempDir::new().unwrap();
    let objs = distinct(4);
    let good = &objs[0];
    let rec = encode_record(good.key, &good.data).unwrap();
    let mut body = header_at(VERSION_BASE).to_vec();
    body.extend_from_slice(&rec);
    let entries = [
        IndexEntry {
            k: good.key,
            off: MAGIC_HEADER.len() as u64,
            slen: good.data.len() as u32 + (1 << 20), // past the body
        },
        IndexEntry {
            k: objs[1].key,
            off: MAGIC_HEADER.len() as u64,
            slen: 0,
        },
    ];
    let footer = build_footer(body.len() as u64, &entries).unwrap();
    body.extend_from_slice(&footer);
    fs::write(dir.path().join(segment_name(1, SEALED_SUFFIX)), &body).unwrap();
    sealed_file(dir.path(), 2, &[&objs[2], &objs[3]]);
    let s = open(dir.path());
    let err = s
        .compact(live_of(&[good, &objs[2]]), verified())
        .unwrap_err();
    assert!(
        err.to_string().contains("index entry out of bounds"),
        "{err}"
    );
    let stats = skip_stats(&err);
    assert_eq!(stats.skipped[0].id, 1);
    assert_eq!(stats.skipped[0].key, good.key);
    assert_eq!(stats.victims, [2]);
    assert!(seg_exists(dir.path(), 1) && !seg_exists(dir.path(), 2));
    must_get(&s, &objs[2]);
}

/// Carrying on is for bad records. A store that cannot write is another matter: the
/// copies are not all there, so nothing may be removed.
#[test]
fn compact_append_failure_removes_nothing() {
    let dir = TempDir::new().unwrap();
    let objs = distinct(6);
    sealed_file(dir.path(), 1, &[&objs[0], &objs[1], &objs[4]]);
    sealed_file(dir.path(), 2, &[&objs[2], &objs[3], &objs[5]]);
    let s = Arc::new(open(dir.path()));
    let failing = Arc::clone(&s);
    let opts = CompactOpts {
        pace: Some(Box::new(move |_| {
            failing.set_failed(&std::io::Error::other("boom"));
        })),
        ..CompactOpts::default()
    };
    let err = s.compact(live_of(&refs(&objs[..4])), opts).unwrap_err();
    assert!(err.to_string().contains("boom"), "{err}");
    assert!(err.compact_stats().is_none(), "reported as a skip: {err}");
    assert!(seg_exists(dir.path(), 1) && seg_exists(dir.path(), 2));
}

/// A victim with nothing live needs no reads, and is not held up by a bad neighbour.
#[test]
fn compact_reaps_a_fully_dead_victim_next_to_a_bad_one() {
    let dir = TempDir::new().unwrap();
    let objs = distinct(4);
    let payload = sealed_file(dir.path(), 1, &[&objs[0], &objs[1]]);
    damage(dir.path(), 1, payload[&objs[0].key]);
    sealed_file(dir.path(), 2, &[&objs[2], &objs[3]]);
    let s = open(dir.path());
    let err = s.compact(live_of(&[&objs[0]]), verified()).unwrap_err();
    assert_eq!(skip_stats(&err).victims, [2]);
    assert!(!seg_exists(dir.path(), 2));
}

/// Going on past a bad record is safe only if the mark verified what it read. Any other
/// walk may have read a bad directory as a smaller one, and the record that shows it up
/// may be another one beside it: a pass that gave up on the pack would never look at the
/// directory. So without a verified mark every bad record stops the pass, of whatever
/// type, with nothing removed.
#[test]
fn compact_without_a_verified_mark_stops_on_any_bad_record() {
    let typed = |t: Type, data: &[u8]| Object {
        key: Key::new(t, data.len() as u64, data),
        data: data.to_vec(),
        record: None,
    };
    let body = [b'x'; 64];
    let cases = [
        ("Blob", typed(Type::Blob, &body)),
        ("XattrSet", typed(Type::XattrSet, &body)),
        ("DirLeaf", typed(Type::DirLeaf, &body)),
        ("DirNode", typed(Type::DirNode, &body)),
        ("FileNode", typed(Type::FileNode, &body)),
        ("Commit", typed(Type::Commit, &body)),
        ("git", git_blob_obj(&body)),
    ];
    for (name, bad) in cases {
        let objs = distinct(3);
        let setup = || {
            let dir = TempDir::new().unwrap();
            let payload = sealed_file(dir.path(), 1, &[&bad, &objs[0]]);
            damage(dir.path(), 1, payload[&bad.key]);
            sealed_file(dir.path(), 2, &[&objs[1], &objs[2]]);
            dir
        };
        let live = [&bad, &objs[1]];

        let dir = setup();
        let s = open(dir.path());
        let err = s
            .compact(live_of(&live), CompactOpts::default())
            .unwrap_err();
        assert!(err.is_corrupt(), "{name}: {err}");
        assert!(err.compact_stats().is_none(), "{name}: reported as a skip");
        assert!(
            seg_exists(dir.path(), 1) && seg_exists(dir.path(), 2),
            "{name}"
        );

        // A caller whose mark verified what it read knows nothing was hidden.
        let dir = setup();
        let s = open(dir.path());
        let opts = CompactOpts {
            mark_verified: true,
            ..CompactOpts::default()
        };
        let err = s.compact(live_of(&live), opts).unwrap_err();
        assert_eq!(skip_stats(&err).victims, [2], "{name}");
        assert!(
            seg_exists(dir.path(), 1) && !seg_exists(dir.path(), 2),
            "{name}"
        );
    }
}

#[test]
fn get_verified() {
    let dir = TempDir::new().unwrap();
    let objs = distinct(2);
    let payload = sealed_file(dir.path(), 1, &refs(&objs));
    damage(dir.path(), 1, payload[&objs[0].key]);
    let s = open(dir.path());
    assert_eq!(s.get_verified(objs[1].key).unwrap(), objs[1].data);
    let err = s.get_verified(objs[0].key).unwrap_err();
    assert!(err.is_verify() && err.is_corrupt(), "{err}");
    let err = s.get_verified(blob_obj(b"absent").key).unwrap_err();
    assert!(err.is_not_found(), "{err}");
}

/// A copy in a pack that is not a victim stands in for the victim's only if it is
/// good. Otherwise the pass removes the only good copy, with no error at all.
#[test]
fn a_bad_survivor_does_not_stand_in_for_the_good_copy() {
    let objs = distinct(3);
    let (k, other, dead) = (&objs[0], &objs[1], &objs[2]);
    let dir = TempDir::new().unwrap();
    let payload = sealed_file(dir.path(), 1, &[k, other]); // nothing dead: never a victim
    sealed_file(dir.path(), 2, &[k, dead]); // the good copy, and garbage
    damage(dir.path(), 1, payload[&k.key]);
    let s = Store::open(dir.path()).unwrap();
    let stats = s
        .compact(live_of(&[k, other]), CompactOpts::default())
        .unwrap();
    assert_eq!(stats.records_copied, 1, "the good one");
    assert_eq!(s.get(k.key).unwrap(), k.data, "the only good copy was lost");

    // And a good one saves the copy.
    let dir = TempDir::new().unwrap();
    sealed_file(dir.path(), 1, &[k, other]);
    sealed_file(dir.path(), 2, &[k, dead]);
    let s = Store::open(dir.path()).unwrap();
    let stats = s
        .compact(live_of(&[k, other]), CompactOpts::default())
        .unwrap();
    assert_eq!(stats.records_copied, 0, "copied past a good survivor");
}

/// The same for another writer's active segment.
#[test]
fn a_bad_copy_in_a_foreign_segment_does_not_stand_in() {
    let dir = TempDir::new().unwrap();
    let objs = distinct(2);
    let (k, dead) = (&objs[0], &objs[1]);
    sealed_file(dir.path(), 1, &[k, dead]);

    let w = Store::open(dir.path()).unwrap(); // another store, alive, its segment synced
    let rec = encode_record(k.key, &k.data).unwrap();
    w.append(k.key, &rec, true).unwrap();
    flip_byte(
        &only_active(dir.path()),
        (MAGIC_HEADER.len() + REC_HEADER_SIZE) as u64,
    );

    let s = Store::open(dir.path()).unwrap();
    let stats = s.compact(live_of(&[k]), CompactOpts::default()).unwrap();
    assert_eq!(stats.records_copied, 1, "{stats:?}");
    assert_eq!(stats.victims, [1], "{stats:?}");
}

/// A crashed seal somebody holds may not be fsynced yet, so it does not stand in for a
/// victim's copy either.
#[test]
fn a_crashed_seal_does_not_stand_in() {
    let dir = TempDir::new().unwrap();
    let objs = distinct(3);
    let (k, other, dead) = (&objs[0], &objs[1], &objs[2]);
    let w = Store::open(dir.path()).unwrap();
    for o in [k, other] {
        w.put_verified_deferred(o.key, &o.data).unwrap();
    }
    *unpoison(w.hooks.after_footer.lock()) = Some(Box::new(|| {
        Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "killed",
        ))
    }));
    {
        let mut ap = w.append_lock();
        w.seal_active(&mut ap).expect_err("the seal went through");
    }
    sealed_file(dir.path(), 2, &[k, dead]); // w holds the file of id 1: the good copy is this
    let s = Store::open(dir.path()).unwrap();
    let stats = s
        .compact(live_of(&[k, other]), CompactOpts::default())
        .unwrap();
    assert_eq!(stats.records_copied, 1, "{stats:?}");
    assert_eq!(stats.victims, [2], "{stats:?}");
}

/// A pack left in place is charged its whole live size though it copies next to
/// nothing, so under a budget it could starve the packs behind it for good.
#[test]
fn a_bad_pack_does_not_eat_the_copy_budget() {
    let (dir, live, _) = three_victims();
    let s = open(dir.path());
    let budget = (REC_HEADER_SIZE + live[0].data.len()) as u64; // one pack's live bytes
    for _ in 0..6 {
        let opts = CompactOpts {
            max_copy_bytes: budget,
            ..verified()
        };
        let _ = s.compact(live_of(&refs(&live)), opts);
    }
    assert!(!seg_exists(dir.path(), 2) && !seg_exists(dir.path(), 3));
    assert!(seg_exists(dir.path(), 1), "the bad pack went");
}

/// It reports when it is a victim: what the line and the budget leave out, it cannot.
#[test]
fn a_bad_pack_reports_when_it_is_a_victim() {
    let dir = TempDir::new().unwrap();
    let objs = distinct(3);
    let payload = sealed_file(dir.path(), 1, &refs(&objs));
    damage(dir.path(), 1, payload[&objs[0].key]);
    let s = open(dir.path());
    let live = [&objs[0], &objs[1]]; // a third dead
    let at = |line| CompactOpts {
        min_dead_ratio: line,
        ..verified()
    };
    s.compact(live_of(&live), at(0.0))
        .expect_err("no report at a line it is above");
    s.compact(live_of(&live), at(0.9))
        .expect("a report at a line it is below");
}

/// The core promise: survivors are durable before anything is unlinked.
#[test]
fn copies_are_durable_before_any_victim_goes() {
    let dir = TempDir::new().unwrap();
    let objs = distinct(4);
    let payload = sealed_file(dir.path(), 1, &[&objs[0], &objs[1]]);
    damage(dir.path(), 1, payload[&objs[0].key]);
    sealed_file(dir.path(), 2, &[&objs[2], &objs[3]]);
    let s = Store::open(dir.path()).unwrap();
    let path = dir.path().to_path_buf();
    let checked = Arc::new(AtomicU32::new(0));
    let seen = Arc::clone(&checked);
    // What fsyncs are asked for, and when: the copies' before the unlink, the directory's
    // after it.
    let (copies_synced, dir_synced_after) =
        (Arc::new(AtomicU32::new(0)), Arc::new(AtomicU32::new(0)));
    {
        let (copies, after, seen) = (
            Arc::clone(&copies_synced),
            Arc::clone(&dir_synced_after),
            Arc::clone(&checked),
        );
        let dir_path = dir.path().to_path_buf();
        *unpoison(s.hooks.before_fsync.lock()) = Some(Box::new(move |p: &Path| {
            let gone = seen.load(Ordering::SeqCst) > 0;
            if !gone && p.to_string_lossy().ends_with(".seg.active") && fs::metadata(p)?.len() > 8 {
                copies.fetch_add(1, Ordering::SeqCst);
            }
            if gone && p == dir_path {
                after.fetch_add(1, Ordering::SeqCst);
            }
            Ok(())
        }));
    }
    let copies = Arc::clone(&copies_synced);
    *unpoison(s.hooks.after_detach.lock()) = Some(Box::new(move || {
        seen.fetch_add(1, Ordering::SeqCst);
        assert!(
            copies.load(Ordering::SeqCst) > 0,
            "victims leaving before the fsync of what was copied out of them"
        );
        let active = only_active(&path);
        let (recs, _) = read_sidecar_file(&sidecar_of(&active));
        let durable = recs
            .iter()
            .filter(|r| r.kind == SIDECAR_SYNCED)
            .map(|r| r.off)
            .max()
            .unwrap_or(0);
        let len = fs::metadata(&active).unwrap().len();
        assert!(
            durable >= len,
            "victims leaving with {durable} of {len} known durable"
        );
    }));
    s.compact(live_of(&[&objs[0], &objs[2]]), verified())
        .unwrap_err();
    assert_eq!(checked.load(Ordering::SeqCst), 1, "nothing was removed");
    assert!(
        dir_synced_after.load(Ordering::SeqCst) > 0,
        "no fsync of the directory after the unlink"
    );
}

/// The stats say what was removed, also when an unlink fails.
#[test]
fn compact_stats_after_a_failed_remove() {
    let dir = TempDir::new().unwrap();
    let objs = distinct(4);
    sealed_file(dir.path(), 1, &[&objs[0], &objs[1]]);
    sealed_file(dir.path(), 2, &[&objs[2], &objs[3]]);
    let s = open(dir.path());
    let first = dir.path().join(segment_name(1, SEALED_SUFFIX));
    *unpoison(s.hooks.after_detach.lock()) = Some(Box::new(move || {
        let _ = fs::remove_file(&first);
    }));
    // Go returns the stats beside this error, and Rust's Result cannot: what the pass
    // did is then in the view.
    let err = s
        .compact(live_of(&[&objs[0], &objs[2]]), CompactOpts::default())
        .unwrap_err();
    assert!(
        err.to_string().to_lowercase().contains("no such file"),
        "{err}"
    );
}

/// And what was copied: a key in two victims is appended once.
#[test]
fn compact_counts_what_it_appended() {
    let dir = TempDir::new().unwrap();
    let objs = distinct(66);
    let mut a = refs(&objs[..64]);
    a.push(&objs[64]);
    let mut b = refs(&objs[..64]);
    b.push(&objs[65]);
    sealed_file(dir.path(), 1, &a);
    sealed_file(dir.path(), 2, &b);
    let s = Store::open_with(dir.path(), Options::new().sync(false).segment_size(1 << 30)).unwrap();
    let stats = s
        .compact(live_of(&refs(&objs[..64])), CompactOpts::default())
        .unwrap();
    assert_eq!(stats.records_copied, 64);
}

/// Two bad records in one pack, as a damaged region of a disk leaves: one report, and
/// nothing else in the pack goes anywhere it should not.
#[test]
fn compact_two_bad_records_in_one_pack() {
    let dir = TempDir::new().unwrap();
    let objs = distinct(5);
    let payload = sealed_file(dir.path(), 1, &[&objs[0], &objs[1], &objs[2], &objs[4]]);
    damage(dir.path(), 1, payload[&objs[0].key]);
    damage(dir.path(), 1, payload[&objs[1].key]);
    sealed_file(dir.path(), 2, &[&objs[3], &objs[4]]);
    let s = open(dir.path());
    let err = s
        .compact(live_of(&refs(&objs[..4])), verified())
        .unwrap_err();
    let stats = skip_stats(&err);
    assert_eq!(stats.skipped.len(), 1, "{:?}", stats.skipped);
    assert_eq!(stats.skipped[0].id, 1);
    let k = stats.skipped[0].key;
    assert!(
        k == objs[0].key || k == objs[1].key,
        "the report names a good record"
    );
    assert!(seg_exists(dir.path(), 1) && !seg_exists(dir.path(), 2));
    must_get(&s, &objs[2]);
    must_get(&s, &objs[3]);
}

/// An entry pointing into the header is out of bounds too, and is told apart from a
/// record that fails its checks.
#[test]
fn compact_names_an_index_entry_in_the_header() {
    let dir = TempDir::new().unwrap();
    let objs = distinct(2);
    let rec = encode_record(objs[0].key, &objs[0].data).unwrap();
    let mut body = header_at(VERSION_BASE).to_vec();
    body.extend_from_slice(&rec);
    let entries = [
        IndexEntry {
            k: objs[0].key,
            off: 0,
            slen: objs[0].data.len() as u32,
        },
        IndexEntry {
            k: objs[1].key,
            off: MAGIC_HEADER.len() as u64,
            slen: 0,
        },
    ];
    let footer = build_footer(body.len() as u64, &entries).unwrap();
    body.extend_from_slice(&footer);
    fs::write(dir.path().join(segment_name(1, SEALED_SUFFIX)), &body).unwrap();
    let s = open(dir.path());
    let err = s.compact(live_of(&[&objs[0]]), verified()).unwrap_err();
    assert!(
        err.is_corrupt() && err.to_string().contains("index entry out of bounds"),
        "{err}"
    );
}

/// A big blob that sorts first in a pack's index, small blobs, and a small target that
/// sorts last: with one verifier the copy follows that order, so that the big one rotates
/// the segment before the target's turn. Returns them all, the target, and a dead one.
fn ordered_victim(big: usize, n: usize) -> (Vec<Object>, Object, Object) {
    let mut objs: Vec<Object> = (0..n)
        .map(|i| {
            let mut d = format!("small-{i:04}-").into_bytes();
            d.extend_from_slice(&[b'x'; 80]);
            blob_obj(&d)
        })
        .collect();
    let target = objs.iter().max_by_key(|o| o.key).unwrap().clone();
    let min_k = objs.iter().map(|o| o.key).min().unwrap();
    let mut base = incompressible(big);
    for i in 0u32.. {
        base[0] = i as u8;
        base[1] = (i >> 8) as u8;
        base[2] = (i >> 16) as u8;
        let o = blob_obj(&base);
        if o.key < min_k {
            objs.push(o);
            return (objs, target, blob_obj(b"dead"));
        }
    }
    unreachable!()
}

fn one_verifier(s: &Store) {
    *unpoison(s.hooks.copy_workers.lock()) = Some(1);
}

/// Every copy of `o` the view holds, a lookup stopping at the first: how many are good,
/// and how many are not.
fn copies_of(s: &Store, o: &Object) -> (usize, usize) {
    use std::os::unix::fs::FileExt;
    let sh = unpoison(s.shared.read());
    let (mut good, mut bad) = (0, 0);
    let mut note = |ok: bool| {
        if ok {
            good += 1;
        } else {
            bad += 1;
        }
    };
    if let Some(a) = &sh.active
        && let Some(loc) = unpoison(a.index.read()).get(&o.key).copied()
    {
        let mut raw = vec![0; REC_HEADER_SIZE + loc.slen as usize];
        note(a.f.read_exact_at(&mut raw, loc.off).is_ok() && valid_record(o.key, &raw));
    }
    for fa in &sh.foreign {
        let loc = unpoison(fa.state.read()).scan.index.get(&o.key).copied();
        if let Some(loc) = loc {
            note(
                matches!(fa.read_record(o.key, loc), Ok(Lookup::Hit(raw)) if valid_record(o.key, &raw)),
            );
        }
    }
    for g in &sh.sealed {
        if let Some((off, slen)) = g.fv.lookup(&g.mm, o.key) {
            note(indexed_record_valid(g, o.key, off, slen));
        }
    }
    (good, bad)
}

/// What a pass's own segment holds stands in for a victim's copy, on the grounds that the
/// pass verified it. A segment somebody left while the pass ran holds what nobody
/// verified: taking it at a rotation would let a rotted copy stand in, and the good pack go.
#[test]
fn a_pass_does_not_adopt_a_segment_while_it_copies() {
    const SEG_SIZE: u64 = 64 << 10;
    let setup = || {
        let dir = TempDir::new().unwrap();
        let (all, target, dead) = ordered_victim(SEG_SIZE as usize, 60);
        let mut victim: Vec<&Object> = all.iter().collect();
        victim.push(&dead);
        sealed_file(dir.path(), 1, &victim);
        let w = Store::open_with(dir.path(), Options::new().segment_size(SEG_SIZE)).unwrap();
        let rec = encode_record(target.key, &target.data).unwrap();
        w.append(target.key, &rec, true).unwrap();
        flip_byte(
            &only_active(dir.path()),
            (MAGIC_HEADER.len() + REC_HEADER_SIZE) as u64,
        );
        (dir, w, all)
    };
    let check = |s: &Store, dir: &Path, all: &[Object], res: Result<super::CompactStats, Error>| {
        let stats = res.unwrap();
        assert!(!seg_exists(dir, 1), "the victim is still there: {stats:?}");
        for o in all {
            assert!(copies_of(s, o).0 > 0, "no good copy left of {}", o.key);
        }
    };

    // A writer exits during the pass.
    let (dir, w, all) = setup();
    let s = Store::open_with(dir.path(), Options::new().segment_size(SEG_SIZE)).unwrap();
    one_verifier(&s);
    let w = Mutex::new(Some(w));
    let w = Arc::new(w);
    let closer = Arc::clone(&w);
    let opts = CompactOpts {
        pace: Some(Box::new(move |_| {
            // The big copy rotated: the next append would adopt its segment.
            if let Some(w) = unpoison(closer.lock()).take() {
                w.close().unwrap();
            }
        })),
        ..CompactOpts::default()
    };
    let res = s.compact(live_of(&refs(&all)), opts);
    check(&s, dir.path(), &all, res);

    // A torn segment keeps the idle sealing from reaching it.
    let (dir, w, all) = setup();
    w.close().unwrap();
    // A writer that died on its first record: nothing valid, but the largest file.
    let mut torn = header_at(VERSION_BASE).to_vec();
    torn.extend_from_slice(&[0x7a; 8 << 10]);
    fs::write(dir.path().join(segment_name(99, ACTIVE_SUFFIX)), torn).unwrap();
    let s = Store::open_with(dir.path(), Options::new().segment_size(SEG_SIZE)).unwrap();
    one_verifier(&s);
    let res = s.compact(live_of(&refs(&all)), CompactOpts::default());
    check(&s, dir.path(), &all, res);
}

/// A crashed seal somebody holds is a victim like any other. When the holder exits during
/// the pass, adopting the file would finish the seal under the pass.
#[test]
fn a_pass_does_not_finish_a_seal_it_is_reading() {
    let dir = TempDir::new().unwrap();
    let w = Store::open_with(dir.path(), Options::new().segment_size(1 << 30)).unwrap();
    let objs: Vec<Object> = (0..200)
        .map(|i| {
            let mut d = format!("{i:06}").into_bytes();
            d.extend_from_slice(&incompressible(4 << 10));
            blob_obj(&d)
        })
        .collect();
    let dead = blob_obj(b"dead");
    for o in objs.iter().chain([&dead]) {
        w.put_verified_deferred(o.key, &o.data).unwrap();
    }
    *unpoison(w.hooks.after_footer.lock()) = Some(Box::new(|| Err(std::io::Error::other("EIO"))));
    {
        let mut ap = w.append_lock();
        w.seal_active(&mut ap).expect_err("the seal went through");
    }
    let s = Store::open_with(dir.path(), Options::new().segment_size(32 << 10)).unwrap();
    let w = Arc::new(Mutex::new(Some(w)));
    let closer = Arc::clone(&w);
    let opts = CompactOpts {
        pace: Some(Box::new(move |_| {
            if let Some(w) = unpoison(closer.lock()).take() {
                let _ = w.close();
            }
        })),
        ..CompactOpts::default()
    };
    s.compact(live_of(&refs(&objs)), opts).unwrap();
    for o in &objs {
        must_get(&s, o);
    }
}

/// The repair the error names has to repair, wherever the bad copy is.
mod put_verified_reaches_what_lookups_read {
    use super::*;

    const NEIGHBOUR: &[u8] = b"a neighbour";

    /// A bad copy of `o` in an active segment nobody holds, next to a good record:
    /// repairing it must not lose its neighbours. Returns the segment's path.
    fn bad_copy(dir: &Path, o: &Object) -> std::path::PathBuf {
        let w = Store::open(dir).unwrap();
        let rec = encode_record(o.key, &o.data).unwrap();
        w.append(o.key, &rec, true).unwrap();
        let path = unpoison(w.shared.read())
            .active
            .as_ref()
            .unwrap()
            .path
            .clone();
        let other = blob_obj(NEIGHBOUR);
        w.put(other.key, &other.data).unwrap();
        w.close().unwrap();
        flip_byte(&path, (MAGIC_HEADER.len() + REC_HEADER_SIZE + 1) as u64);
        path
    }

    fn verified_ok(s: &Store, o: &Object) {
        assert_eq!(s.get_verified(o.key).unwrap(), o.data);
        assert_eq!(copies_of(s, o).1, 0, "bad copies left");
        assert_eq!(s.get(blob_obj(NEIGHBOUR).key).unwrap(), NEIGHBOUR);
    }

    fn reopened_ok(dir: &Path, o: &Object) {
        let r = Store::open(dir).unwrap();
        verified_ok(&r, o);
    }

    /// The first write after an open used to adopt the segment on its way to appending,
    /// and take the bad record for one already there.
    #[test]
    fn the_only_copy_in_a_segment_nobody_holds() {
        let o = &test_objects(1)[0];
        let dir = TempDir::new().unwrap();
        bad_copy(dir.path(), o);
        let s = Store::open(dir.path()).unwrap();
        s.put_verified(o.key, &o.data).unwrap();
        verified_ok(&s, o);
        s.close().unwrap();
        reopened_ok(dir.path(), o);
    }

    /// The good sealed copy used to satisfy it, and lookups read the other one first.
    #[test]
    fn beside_a_good_sealed_copy() {
        let o = &test_objects(1)[0];
        let dir = TempDir::new().unwrap();
        sealed_file(dir.path(), 1, &[o]);
        bad_copy(dir.path(), o);
        let s = Store::open(dir.path()).unwrap();
        s.put_verified(o.key, &o.data).unwrap();
        verified_ok(&s, o);
        s.close().unwrap();
        reopened_ok(dir.path(), o);
    }

    #[test]
    fn in_two_segments_nobody_holds() {
        let o = &test_objects(1)[0];
        let dir = TempDir::new().unwrap();
        bad_copy(dir.path(), o);
        // Another segment, held open meanwhile so that the second writer makes its own.
        let h = Store::open(dir.path()).unwrap();
        let hold = blob_obj(b"hold");
        h.put(hold.key, &hold.data).unwrap();
        bad_copy(dir.path(), o);
        h.close().unwrap();
        let s = Store::open(dir.path()).unwrap();
        s.put_verified(o.key, &o.data).unwrap();
        verified_ok(&s, o);
        s.close().unwrap();
        reopened_ok(dir.path(), o);
    }

    /// A store that has written already does not look at the directory again by itself,
    /// and has to: this call answers for every copy.
    #[test]
    fn in_a_segment_this_store_has_not_seen_yet() {
        let o = &test_objects(1)[0];
        let dir = TempDir::new().unwrap();
        let s = Store::open(dir.path()).unwrap();
        let own = blob_obj(b"the store's own");
        s.put(own.key, &own.data).unwrap();
        bad_copy(dir.path(), o);
        s.put_verified(o.key, &o.data).unwrap();
        s.refresh().unwrap();
        verified_ok(&s, o);
    }

    /// Nothing can be done to a segment a live writer holds, and saying nothing would be
    /// a lie: lookups go on reading the bad copy.
    #[test]
    fn in_a_segment_a_live_writer_holds() {
        let o = &test_objects(1)[0];
        let dir = TempDir::new().unwrap();
        let w = Store::open(dir.path()).unwrap();
        let rec = encode_record(o.key, &o.data).unwrap();
        w.append(o.key, &rec, true).unwrap();
        flip_byte(
            &only_active(dir.path()),
            (MAGIC_HEADER.len() + REC_HEADER_SIZE + 1) as u64,
        );
        let s = Store::open(dir.path()).unwrap();
        let err = s
            .put_verified(o.key, &o.data)
            .expect_err("acknowledged a repair of a copy that is still bad");
        for want in [
            o.key.to_string().as_str(),
            "0000000000000001",
            "another store",
        ] {
            assert!(
                err.to_string().contains(want),
                "does not say {want:?}: {err}"
            );
        }
        w.close().unwrap();
        // And once it has let go, the same call does the job.
        s.put_verified(o.key, &o.data).unwrap();
        assert_eq!(s.get_verified(o.key).unwrap(), o.data);
    }

    /// A record whose own header went bad does not even say it is that record.
    #[test]
    fn with_a_bad_header() {
        let o = &test_objects(1)[0];
        let dir = TempDir::new().unwrap();
        let p = bad_copy(dir.path(), o);
        flip_byte(&p, (MAGIC_HEADER.len() + 1) as u64); // in the key
        let s = Store::open(dir.path()).unwrap();
        s.put_verified(o.key, &o.data).unwrap();
        verified_ok(&s, o);
    }

    /// Whoever held it has sealed it since this view looked: the view catches up, and the
    /// repair is done on the sealed segment.
    #[test]
    fn sealed_under_the_view() {
        let o = &test_objects(1)[0];
        let dir = TempDir::new().unwrap();
        let p = bad_copy(dir.path(), o);
        let s = Store::open(dir.path()).unwrap();
        let other = Store::open(dir.path()).unwrap();
        {
            let mut ap = other.append_lock();
            other.seal_idle(&mut ap).unwrap(); // seals what nobody holds
        }
        assert!(!p.exists(), "the segment is still active");
        {
            let mut ap = s.append_lock();
            s.take_bad_foreign_copies(&mut ap, o.key, false).unwrap();
        }
        {
            let sh = unpoison(s.shared.read());
            assert_eq!(
                (sh.sealed.len(), sh.foreign.len()),
                (1, 0),
                "the view has not caught up"
            );
        }
        s.put_verified(o.key, &o.data).unwrap();
        verified_ok(&s, o);
    }

    /// A good foreign copy needs nothing.
    #[test]
    fn a_good_copy_in_a_segment_a_live_writer_holds() {
        let o = &test_objects(1)[0];
        let dir = TempDir::new().unwrap();
        let w = Store::open(dir.path()).unwrap();
        w.put(o.key, &o.data).unwrap();
        let s = Store::open(dir.path()).unwrap();
        s.put_verified(o.key, &o.data).unwrap();
        assert_eq!(s.get_verified(o.key).unwrap(), o.data);
        w.close().unwrap();
    }
}

/// A good copy elsewhere must not hide it: lookups read the higher id first.
#[test]
fn put_verified_does_not_acknowledge_beside_a_bad_held_crashed_seal() {
    let dir = TempDir::new().unwrap();
    let objs = test_objects(2);
    sealed_file(dir.path(), 1, &[&objs[0]]);
    let w = Store::open(dir.path()).unwrap();
    for o in &objs {
        let rec = encode_record(o.key, &o.data).unwrap();
        w.append(o.key, &rec, true).unwrap();
    }
    let path = only_active(dir.path());
    *unpoison(w.hooks.after_footer.lock()) = Some(Box::new(|| {
        Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "killed",
        ))
    }));
    {
        let mut ap = w.append_lock();
        w.seal_active(&mut ap).expect_err("the seal went through");
    }
    flip_byte(&path, (MAGIC_HEADER.len() + REC_HEADER_SIZE) as u64);
    let s = Store::open(dir.path()).unwrap();
    let err = s.put_verified(objs[0].key, &objs[0].data).unwrap_err();
    assert!(err.to_string().contains("another store holds"), "{err}");
}

/// A pass copies into a new segment. An empty one it could have taken has an id below the
/// pack that survives with a bad copy: once sealed, that pack would be read first, and
/// `get` does not check what it reads.
#[test]
fn a_pass_copies_above_every_pack_that_survives() {
    let dir = TempDir::new().unwrap();
    let objs = distinct(3);
    let (x, other, dead) = (&objs[0], &objs[1], &objs[2]);
    let e = Store::open(dir.path()).unwrap();
    {
        let mut ap = e.append_lock();
        e.ensure_active(&mut ap).unwrap();
    }
    e.close().unwrap();
    // Nothing dead in 2, so never a victim, and its copy of x goes bad.
    let payload = sealed_file(dir.path(), 2, &[x, other]);
    sealed_file(dir.path(), 3, &[x, dead]); // the good copy
    damage(dir.path(), 2, payload[&x.key]);
    let s = open(dir.path());
    s.compact(live_of(&[x, other]), verified()).unwrap();
    {
        let mut ap = s.append_lock();
        s.seal_active(&mut ap).unwrap();
    }
    assert_eq!(s.get_verified(x.key).unwrap(), x.data);
}

/// An empty one does not end the sealing. A torn file is the fullest by size and empty
/// once recovered, so it would come first.
#[test]
fn a_pass_seals_every_idle_segment_past_an_empty_one() {
    let dir = TempDir::new().unwrap();
    let objs = distinct(2);
    let w = open(dir.path());
    w.put(objs[0].key, &objs[0].data).unwrap();
    w.close().unwrap();
    let mut torn = header_at(VERSION_BASE).to_vec();
    torn.extend_from_slice(&[0x7a; 64 << 10]);
    fs::write(dir.path().join(segment_name(99, ACTIVE_SUFFIX)), torn).unwrap();
    let s = open(dir.path());
    s.compact(live_of(&[&objs[0]]), CompactOpts::default())
        .unwrap();
    assert!(
        seg_exists(dir.path(), 1),
        "an idle segment with an object in it was left unsealed"
    );
}

/// "Call again when the writer has let go", with writes in between: each try must not
/// seal the caller's segment on its way to finding out.
#[test]
fn a_refused_repair_leaves_the_callers_segment_alone() {
    let dir = TempDir::new().unwrap();
    let objs = distinct(3);
    let o = &objs[0];
    let w = Store::open(dir.path()).unwrap();
    let rec = encode_record(o.key, &o.data).unwrap();
    w.append(o.key, &rec, true).unwrap();
    flip_byte(
        &only_active(dir.path()),
        (MAGIC_HEADER.len() + REC_HEADER_SIZE) as u64,
    );
    let s = Store::open(dir.path()).unwrap();
    s.put(objs[1].key, &objs[1].data).unwrap();
    let id = s.append_lock().active.as_ref().unwrap().seg.id;
    let err = s.put_verified(o.key, &o.data).unwrap_err();
    assert!(err.to_string().contains("another store holds"), "{err}");
    assert_eq!(
        s.append_lock().active.as_ref().map(|a| a.seg.id),
        Some(id),
        "the caller's segment was given up"
    );
}

/// A verifying ingest writes to the directory it looks at, and a directory changed within
/// the racy window is listed every time it is looked at.
#[test]
fn deferred_puts_look_at_the_directory_once_a_second() {
    use super::repair::DEFERRED_LOOK_EVERY;
    let dir = TempDir::new().unwrap();
    for i in 0..8u8 {
        sealed_file(dir.path(), u64::from(i) + 1, &[&blob_obj(&[i, 1, 2, 3])]);
    }
    let s = Store::open(dir.path()).unwrap();
    let objs = distinct(100);
    let before = s.refreshes.load(Ordering::Relaxed);
    let start = std::time::Instant::now();
    for o in &objs {
        s.put_verified_deferred(o.key, &o.data).unwrap();
    }
    let n = s.refreshes.load(Ordering::Relaxed) - before;
    let limit = 3 + start.elapsed().as_secs();
    assert!(n <= limit, "{n} listings for 100 puts, limit {limit}");
    // A second on, with nothing else having ended anything, it sees what appeared meanwhile.
    let o = blob_obj(b"in between");
    let payload = sealed_file(dir.path(), 50, &[&o]);
    damage(dir.path(), 50, payload[&o.key]);
    {
        let mut sh = unpoison(s.shared.write());
        sh.listed_at -= DEFERRED_LOOK_EVERY;
    }
    s.put_verified_deferred(o.key, &o.data).unwrap();
    s.sync().unwrap();
    s.refresh().unwrap();
    assert_eq!(copies_of(&s, &o).1, 0, "a bad copy left");
    assert_eq!(s.get_verified(o.key).unwrap(), o.data);
}

/// A repair works from the view's mapping, and copies the rest of the pack from there. One
/// from a mapping older than another store's repair would undo that repair.
#[test]
fn a_deferred_put_does_not_undo_a_repair_another_store_made() {
    for deferred in [true, false] {
        let dir = TempDir::new().unwrap();
        let objs = distinct(3);
        let (k1, k2, other) = (&objs[0], &objs[1], &objs[2]);
        let payload = sealed_file(dir.path(), 1, &[k1, k2]);
        damage(dir.path(), 1, payload[&k1.key]);
        damage(dir.path(), 1, payload[&k2.key]);
        let b = Store::open(dir.path()).unwrap();
        b.put_verified_deferred(other.key, &other.data).unwrap();
        let a = Store::open(dir.path()).unwrap();
        a.put_verified(k1.key, &k1.data).unwrap();
        if deferred {
            b.put_verified_deferred(k2.key, &k2.data).unwrap();
        } else {
            b.put_verified(k2.key, &k2.data).unwrap();
        }
        b.sync().unwrap();
        let r = Store::open(dir.path()).unwrap();
        assert_eq!(r.get_verified(k1.key).unwrap(), k1.data, "{deferred}");
        assert_eq!(r.get_verified(k2.key).unwrap(), k2.data, "{deferred}");
    }
}

/// Refused, because a store holds a bad crashed seal. "Call again" must work as soon as it
/// has gone, however soon that is.
#[test]
fn a_refused_deferred_put_goes_through_when_the_holder_has_gone() {
    let objs = test_objects(2);
    let dir = TempDir::new().unwrap();
    let w = Store::open(dir.path()).unwrap();
    let rec = encode_record(objs[0].key, &objs[0].data).unwrap();
    w.append(objs[0].key, &rec, true).unwrap();
    let path = only_active(dir.path());
    *unpoison(w.hooks.after_footer.lock()) = Some(Box::new(|| {
        Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "killed",
        ))
    }));
    {
        let mut ap = w.append_lock();
        w.seal_active(&mut ap).expect_err("the seal went through");
    }
    flip_byte(&path, (MAGIC_HEADER.len() + REC_HEADER_SIZE) as u64);
    let s = Store::open(dir.path()).unwrap();
    let err = s
        .put_verified_deferred(objs[0].key, &objs[0].data)
        .unwrap_err();
    assert!(err.to_string().contains("another store holds"), "{err}");
    let _ = w.close();
    let c = Store::open(dir.path()).unwrap();
    c.put(objs[1].key, &objs[1].data).unwrap(); // adopts, and so finishes the seal
    s.put_verified_deferred(objs[0].key, &objs[0].data).unwrap();
    assert_eq!(s.get_verified(objs[0].key).unwrap(), objs[0].data);
}

/// The same for a bad copy in somebody's active segment: a deferred put does not act on
/// what it remembers of that one either.
#[test]
fn a_deferred_put_looks_before_taking_over_a_foreign_segment() {
    let dir = TempDir::new().unwrap();
    let objs = distinct(2);
    let o = &objs[0];
    let s = Store::open(dir.path()).unwrap();
    s.put(objs[1].key, &objs[1].data).unwrap();
    let w = Store::open(dir.path()).unwrap();
    let rec = encode_record(o.key, &o.data).unwrap();
    w.append(o.key, &rec, true).unwrap();
    let path = active_files(dir.path())
        .into_iter()
        .max()
        .expect("no active segment");
    flip_byte(&path, (MAGIC_HEADER.len() + REC_HEADER_SIZE) as u64);
    s.refresh().unwrap(); // the view now holds the bad copy, and has just looked
    let before = s.refreshes.load(Ordering::Relaxed);
    let err = s.put_verified_deferred(o.key, &o.data).unwrap_err();
    assert!(err.to_string().contains("another store holds"), "{err}");
    assert_ne!(
        s.refreshes.load(Ordering::Relaxed),
        before,
        "it refused on the strength of what it remembered"
    );
}

/// A repair moves no generation, so nothing else makes a long-lived store look again.
#[test]
fn get_verified_sees_a_repair_another_store_made() {
    let dir = TempDir::new().unwrap();
    let objs = distinct(2);
    let k = &objs[0];
    let payload = sealed_file(dir.path(), 1, &refs(&objs));
    damage(dir.path(), 1, payload[&k.key]);
    let b = open(dir.path());
    let err = b.get_verified(k.key).unwrap_err();
    assert!(err.is_corrupt() && err.is_verify(), "{err}");
    let a = open(dir.path());
    a.put_verified(k.key, &k.data).unwrap();
    assert_eq!(b.get_verified(k.key).unwrap(), k.data);
}

/// Between its two looks at a damaged copy, another store repairs it. There is nothing
/// left to do, and that is no failure.
#[test]
fn a_repair_another_store_made_in_the_meantime_is_no_failure() {
    let dir = TempDir::new().unwrap();
    let objs = distinct(3);
    let (k, neighbour, own) = (objs[0].clone(), objs[1].clone(), objs[2].clone());
    let payload = sealed_file(dir.path(), 1, &[&k, &neighbour]);
    damage(dir.path(), 1, payload[&k.key]);
    let s = Arc::new(Store::open(dir.path()).unwrap());
    s.put(own.key, &own.data).unwrap();
    let a = Store::open(dir.path()).unwrap();
    let (s2, k2) = (Arc::downgrade(&s), k.clone());
    *unpoison(s.hooks.after_footer.lock()) = Some(Box::new(move || {
        a.put_verified(k2.key, &k2.data).unwrap();
        s2.upgrade().unwrap().refresh().unwrap();
        Ok(())
    }));
    s.put_verified(k.key, &k.data).unwrap();
    assert_eq!(s.get_verified(k.key).unwrap(), k.data);
}
