//! What a store that syncs may acknowledge (Go: `packstore/durable_dedup_test.go`).

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use tempfile::TempDir;

use super::compression_tests::distinct;
use super::sidecar::SIDECAR_SYNCED;
use super::store_tests::obj_seq;
use super::testutil::*;
use super::view::segment_name;
use super::{ACTIVE_SUFFIX, Object, Options, SEALED_SUFFIX, Store, WriteOpts, unpoison};

fn copy_dir(src: &Path) -> TempDir {
    let dst = TempDir::new().unwrap();
    for e in fs::read_dir(src).unwrap() {
        let e = e.unwrap();
        fs::copy(e.path(), dst.path().join(e.file_name())).unwrap();
    }
    dst
}

/// A directory in the state a writer's death leaves: its active segment holding
/// `objs`, appended and never synced, and nobody holding its locks (Go: `killedWriter`).
fn killed_writer(objs: &[Object]) -> TempDir {
    let src = TempDir::new().unwrap();
    let w = Store::open(src.path()).unwrap();
    for o in objs {
        w.put_verified_deferred(o.key, &o.data).unwrap();
    }
    copy_dir(src.path())
}

/// How much of the segment the sidecar knows durable, and how long it is.
fn durable_mark(dir: &Path) -> (u64, u64) {
    let active = only_active(dir);
    let (recs, _) = read_sidecar_file(&sidecar_of(&active));
    let durable = recs
        .iter()
        .filter(|r| r.kind == SIDECAR_SYNCED)
        .map(|r| r.off)
        .max()
        .unwrap_or(0);
    (durable, fs::metadata(&active).unwrap().len())
}

fn fsyncs(s: &Store) -> u64 {
    s.fsyncs.load(std::sync::atomic::Ordering::SeqCst)
}

fn own_copy(s: &Store, o: &Object) -> bool {
    let sh = unpoison(s.shared.read());
    sh.active
        .as_ref()
        .is_some_and(|a| unpoison(a.index.read()).contains_key(&o.key))
}

fn adopt(dir: &Path, opts: Options) -> Store {
    let s = Store::open_with(dir, opts).unwrap();
    {
        let mut ap = s.append_lock();
        s.ensure_active(&mut ap).unwrap();
    }
    s
}

/// #27. A writer died with records it never synced. A store that takes over its
/// segment now holds them in its own index, and acknowledges what its own index
/// holds on the grounds that its own fsync covers it. So it has to fsync them.
#[test]
fn a_put_against_an_adopted_segment_is_durable() {
    let objs = test_objects(3);
    let dir = killed_writer(&objs);
    let s = Store::open(dir.path()).unwrap();
    for (i, o) in objs.iter().enumerate() {
        s.put(o.key, &o.data).unwrap();
        let (durable, len) = durable_mark(dir.path());
        assert!(durable >= len, "object {i}: {durable} durable of {len}");
    }
    // A retried ingest of the whole tail costs one fsync, not one per object.
    assert_eq!(fsyncs(&s), 1);
}

#[test]
fn a_put_verified_against_an_adopted_segment_is_durable() {
    let objs = test_objects(3);
    let dir = killed_writer(&objs);
    let s = Store::open(dir.path()).unwrap();
    s.put_verified(objs[1].key, &objs[1].data).unwrap();
    let (durable, len) = durable_mark(dir.path());
    assert!(durable >= len, "{durable} durable of {len}");
}

#[test]
fn adoption_syncs_a_tail_its_writer_never_synced() {
    let objs = test_objects(3);
    let dir = killed_writer(&objs);
    let s = adopt(dir.path(), Options::new());
    assert_eq!(fsyncs(&s), 1);
    let (durable, len) = durable_mark(dir.path());
    assert_eq!(durable, len);
}

#[test]
fn adoption_without_a_sidecar_syncs() {
    let objs = test_objects(3);
    let dir = killed_writer(&objs);
    for p in sidecar_files(dir.path()) {
        fs::remove_file(p).unwrap();
    }
    let s = adopt(dir.path(), Options::new());
    assert_eq!(fsyncs(&s), 1);
    let (durable, len) = durable_mark(dir.path());
    assert_eq!(durable, len);
}

/// A process per command pays nothing.
#[test]
fn adopting_what_a_writer_closed_cleanly_does_not_sync() {
    let objs = test_objects(3);
    let dir = TempDir::new().unwrap();
    let w = Store::open(dir.path()).unwrap();
    put_all(&w, &objs);
    w.close().unwrap();
    assert_eq!(fsyncs(&adopt(dir.path(), Options::new())), 0);
}

#[test]
fn adopting_an_empty_segment_does_not_sync() {
    let dir = TempDir::new().unwrap();
    let w = adopt(dir.path(), Options::new());
    w.close().unwrap();
    assert_eq!(fsyncs(&adopt(dir.path(), Options::new())), 0);
}

#[test]
fn a_store_that_does_not_sync_promises_nothing_at_adoption() {
    let objs = test_objects(3);
    let dir = killed_writer(&objs);
    let s = adopt(dir.path(), Options::new().sync(false));
    assert_eq!(fsyncs(&s), 0);
    assert_eq!(durable_mark(dir.path()).0, 0, "a mark nothing synced");
}

/// A writer killed during a seal: a whole footer over a body nobody has fsynced, still
/// under its active name. Returns the directory, and the writer that holds its lock.
fn crashed_seal(objs: &[Object]) -> (TempDir, Store) {
    let dir = TempDir::new().unwrap();
    let w = Store::open(dir.path()).unwrap();
    for o in objs {
        w.put_verified_deferred(o.key, &o.data).unwrap();
    }
    *unpoison(w.hooks.after_footer.lock()) = Some(Box::new(|| {
        Err(io::Error::new(io::ErrorKind::PermissionDenied, "killed"))
    }));
    {
        let mut ap = w.append_lock();
        w.seal_active(&mut ap).expect_err("the seal went through");
    }
    (dir, w)
}

fn active_path(dir: &Path, id: u64) -> PathBuf {
    dir.join(segment_name(id, ACTIVE_SUFFIX))
}

/// #27's other door: a view maps such a file as the sealed segment it is about to be,
/// and a sealed segment counted as durable.
#[test]
fn a_crashed_seal_is_not_acknowledged_unsynced() {
    let objs = test_objects(3);
    let (src, _w) = crashed_seal(&objs);
    let o = &objs[0];
    type Write = fn(&Store, &Object);
    let paths: [(&str, Write); 4] = [
        ("put", |s, o| s.put(o.key, &o.data).unwrap()),
        ("put_verified", |s, o| {
            s.put_verified(o.key, &o.data).unwrap();
        }),
        ("write_batch", |s, o| {
            s.write_batch(obj_seq(std::slice::from_ref(o), None))
                .unwrap();
        }),
        ("write_parallel", |s, o| {
            let (_, r) =
                s.write_parallel(obj_seq(std::slice::from_ref(o), None), WriteOpts::default());
            r.unwrap();
        }),
    ];
    for (name, write) in paths {
        let dir = copy_dir(src.path());
        let s = Store::open(dir.path()).unwrap();
        write(&s, o);
        // Finishing the seal is what fsyncs that file, and renames it.
        assert!(
            !active_path(dir.path(), 1).exists() || own_copy(&s, o),
            "{name}: acknowledged, and the only copy is in a crashed seal nobody fsynced"
        );
        assert_eq!(s.get(o.key).unwrap(), o.data, "{name}");
    }
}

/// What put_verified finds it finishes, and relies on: no second copy.
#[test]
fn put_verified_finishes_a_crashed_seal() {
    let objs = test_objects(3);
    let (src, _w) = crashed_seal(&objs);
    let dir = copy_dir(src.path());
    let s = Store::open(dir.path()).unwrap();
    s.put_verified(objs[0].key, &objs[0].data).unwrap();
    assert!(dir.path().join(segment_name(1, SEALED_SUFFIX)).exists());
    assert!(!own_copy(&s, &objs[0]), "a second copy");
}

/// A store that does not sync promises nothing, and need not write a second copy.
#[test]
fn a_store_that_does_not_sync_relies_on_a_crashed_seal() {
    let objs = test_objects(3);
    let (src, _w) = crashed_seal(&objs);
    let dir = copy_dir(src.path());
    let s = Store::open_with(dir.path(), Options::new().sync(false)).unwrap();
    s.put(objs[0].key, &objs[0].data).unwrap();
    assert!(!own_copy(&s, &objs[0]), "a second copy");
}

/// A crashed seal somebody else has hold of is being finished by them, and may not be
/// fsynced yet. put_verified writes its own copy, and leaves theirs alone.
#[test]
fn a_crashed_seal_held_by_somebody_is_not_relied_on() {
    let objs = test_objects(2);
    let (dir, _w) = crashed_seal(&objs); // _w still holds the file's lock
    let s = Store::open(dir.path()).unwrap();
    s.put_verified(objs[0].key, &objs[0].data).unwrap();
    assert!(own_copy(&s, &objs[0]), "acknowledged against it");
    assert!(
        active_path(dir.path(), 1).exists(),
        "it finished their seal"
    );
}

/// A failed fsync at adoption: the store keeps nothing of the segment and is out of
/// action, and the segment is free for the next one.
#[test]
fn a_failed_adoption_fsync_poisons_the_store() {
    let objs = distinct(2);
    let dir = killed_writer(&objs);
    let s = Store::open(dir.path()).unwrap();
    *unpoison(s.hooks.before_fsync.lock()) = Some(Box::new(|_| Err(io::Error::other("boom"))));
    let err = s.put(objs[0].key, &objs[0].data).unwrap_err();
    assert!(err.to_string().contains("boom"), "{err}");
    assert!(
        unpoison(s.shared.read()).active.is_none(),
        "it took the segment"
    );
    // Out of action for writes, as after any failed fsync: the disk may have dropped
    // what it failed to write, so a retry proves nothing.
    *unpoison(s.hooks.before_fsync.lock()) = None;
    let err = s.put(objs[1].key, &objs[1].data).unwrap_err();
    assert!(err.to_string().contains("boom"), "{err}");
    // And it let go of the lock: the next store adopts it rather than make its own.
    let other = Store::open(dir.path()).unwrap();
    other.put(objs[0].key, &objs[0].data).unwrap();
    let active = unpoison(other.shared.read()).active.clone().unwrap();
    assert_eq!(active.path, active_path(dir.path(), 1));
}

/// Nor does a repair rewrite one: it is somebody else's to finish.
#[test]
fn a_repair_leaves_a_crashed_seal_somebody_holds() {
    let objs = test_objects(2);
    let (dir, w) = crashed_seal(&objs);
    // Its copy is damaged, which a repair would fix if it were this store's to rewrite.
    let path = active_path(dir.path(), 1);
    flip_byte(
        &path,
        (super::MAGIC_HEADER.len() + crate::amberpack::REC_HEADER_SIZE) as u64,
    );
    let before = fs::read(&path).unwrap();
    // A damaged copy of the same object in this store's own segment takes put_verified
    // down its repair path, with the crashed seal in view.
    let s = Store::open(dir.path()).unwrap();
    s.put(objs[0].key, b"wrong").unwrap();
    assert!(own_copy(&s, &objs[0]), "no copy of its own to repair");
    // It answers for every copy, and cannot reach that one: saying nothing would be a lie.
    let err = s.put_verified(objs[0].key, &objs[0].data).unwrap_err();
    assert!(err.to_string().contains("another store holds"), "{err}");
    assert_eq!(fs::read(&path).unwrap(), before, "a repair rewrote it");
    // Once its holder has let go, the same call finishes the seal and repairs it.
    let _ = w.close();
    s.put_verified(objs[0].key, &objs[0].data).unwrap();
    assert!(!path.exists(), "nobody finished the seal");
    assert_eq!(s.get_verified(objs[0].key).unwrap(), objs[0].data);
}

/// The exported form of the same check: "a copy somewhere else" must be one that can
/// stand in for this one.
#[test]
fn has_outside_does_not_rely_on_a_crashed_seal() {
    let objs = test_objects(2);
    let (dir, path) = crashed_seal_let_go(&objs[..1]);
    fs::write(
        dir.path().join(segment_name(7, SEALED_SUFFIX)),
        fs::read(path).unwrap(),
    )
    .unwrap();
    let s = Store::open(dir.path()).unwrap();
    assert!(
        !s.has_outside(7, objs[0].key).unwrap(),
        "the only copy outside segment 7 is in a crashed seal nobody has fsynced"
    );
}

/// The payload of a record is whatever somebody stored. These bytes end the segment, and
/// read as a whole footer that fits exactly where they lie (Go:
/// `crafted_footer_test.go`).
fn object_ending_in_a_footer(s: &Store) -> Object {
    let start = {
        let mut ap = s.append_lock();
        s.ensure_active(&mut ap).unwrap();
        ap.active.as_ref().unwrap().size + crate::amberpack::REC_HEADER_SIZE as u64
    };
    let decoy = blob_obj(b"decoy");
    let footer = super::footer::build_footer(
        start + 1,
        &[super::footer::IndexEntry {
            k: decoy.key,
            off: super::MAGIC_HEADER.len() as u64,
            slen: 0,
        }],
    )
    .unwrap();
    let mut data = vec![b'x'];
    data.extend_from_slice(&footer);
    blob_obj(&data)
}

fn with_a_record_ending_in_a_footer() -> (TempDir, Store, Vec<Object>) {
    let dir = TempDir::new().unwrap();
    let w = Store::open(dir.path()).unwrap();
    let mut all = distinct(3);
    for o in &all {
        w.put(o.key, &o.data).unwrap();
    }
    let evil = object_ending_in_a_footer(&w);
    w.put(evil.key, &evil.data).unwrap();
    all.push(evil);
    (dir, w, all)
}

/// Every object reads, and the next writer appends behind them instead of finishing a
/// seal: renaming the segment would make the decoy index the only one it has.
fn nothing_took_it_for_a_seal(dir: &Path, all: &[Object]) {
    want_objects(&Store::open(dir).unwrap(), all);
    let n = Store::open(dir).unwrap();
    let next = blob_obj(b"the next writer");
    n.put(next.key, &next.data).unwrap();
    assert!(
        active_path(dir, 1).exists(),
        "the next writer finished a seal nobody started"
    );
    n.close().unwrap();
    let a = Store::open(dir).unwrap();
    want_objects(&a, all);
    want_objects(&a, &[next]);
}

#[test]
fn a_record_that_ends_in_a_footer_is_no_seal_after_a_clean_close() {
    let (dir, w, all) = with_a_record_ending_in_a_footer();
    w.close().unwrap();
    nothing_took_it_for_a_seal(dir.path(), &all);
}

#[test]
fn a_record_that_ends_in_a_footer_is_no_seal_without_a_sidecar() {
    let (dir, w, all) = with_a_record_ending_in_a_footer();
    w.close().unwrap();
    let mut sidecar = active_path(dir.path(), 1).into_os_string();
    sidecar.push(super::sidecar::SIDECAR_SUFFIX);
    fs::remove_file(sidecar).unwrap();
    nothing_took_it_for_a_seal(dir.path(), &all);
}

#[test]
fn a_record_that_ends_in_a_footer_is_no_seal_to_another_store() {
    let (dir, _w, all) = with_a_record_ending_in_a_footer();
    want_objects(&Store::open(dir.path()).unwrap(), &all);
}

#[test]
fn a_seal_behind_a_record_that_ends_in_a_footer_is_still_a_seal() {
    let (dir, w, all) = with_a_record_ending_in_a_footer();
    *unpoison(w.hooks.after_footer.lock()) = Some(Box::new(|| {
        Err(io::Error::new(io::ErrorKind::PermissionDenied, "killed"))
    }));
    {
        let mut ap = w.append_lock();
        w.seal_active(&mut ap).expect_err("the seal went through");
    }
    let _ = w.close();
    let n = Store::open(dir.path()).unwrap();
    let next = blob_obj(b"the next writer");
    n.put(next.key, &next.data).unwrap();
    assert!(
        !active_path(dir.path(), 1).exists(),
        "nobody finished the seal"
    );
    want_objects(&n, &all);
    want_objects(&n, &[next]);
}

/// A crashed seal nobody holds any more. Returns the directory and the segment's path,
/// which is still its active name.
fn crashed_seal_let_go(objs: &[Object]) -> (TempDir, PathBuf) {
    let (dir, w) = crashed_seal(objs);
    let _ = w.close();
    let path = active_path(dir.path(), 1);
    (dir, path)
}

/// A failed fsync may have dropped what it failed to write, and a retry reports success,
/// so nothing may be acknowledged after one: finishing a seal is no exception.
#[test]
fn a_failed_fsync_finishing_a_seal_poisons_the_store() {
    let objs = test_objects(2);
    let (dir, path) = crashed_seal_let_go(&objs);
    let s = Store::open(dir.path()).unwrap();
    *unpoison(s.hooks.before_fsync.lock()) = Some(Box::new(move |p: &Path| {
        if p == path {
            return Err(io::Error::other("boom"));
        }
        Ok(())
    }));
    let err = s.put_verified(objs[0].key, &objs[0].data).unwrap_err();
    assert!(err.to_string().contains("boom"), "{err}");
    *unpoison(s.hooks.before_fsync.lock()) = None;
    let err = s
        .put_verified(objs[0].key, &objs[0].data)
        .expect_err("acknowledged on a segment whose fsync failed");
    assert!(err.to_string().contains("boom"), "{err}");
}

/// The same for the directory: without its fsync the rename may not last.
#[test]
fn a_failed_directory_fsync_finishing_a_seal_poisons_the_store() {
    let objs = test_objects(2);
    let (dir, path) = crashed_seal_let_go(&objs);
    let s = Store::open(dir.path()).unwrap();
    *unpoison(s.hooks.before_fsync.lock()) = Some(Box::new(move |_: &Path| {
        if !path.exists() {
            return Err(io::Error::other("boom")); // the rename happened: this is the directory's
        }
        Ok(())
    }));
    let err = s.put_verified(objs[0].key, &objs[0].data).unwrap_err();
    assert!(err.to_string().contains("boom"), "{err}");
    *unpoison(s.hooks.before_fsync.lock()) = None;
    s.put_verified(objs[0].key, &objs[0].data)
        .expect_err("acknowledged on a rename whose fsync failed");
}

/// A seal is trusted from its rename on, so the body is synced before it, and the rename
/// after it.
#[test]
fn finishing_a_seal_syncs_the_body_then_the_rename() {
    let objs = test_objects(1);
    let (dir, path) = crashed_seal_let_go(&objs);
    let s = Store::open(dir.path()).unwrap();
    let seen = Arc::new(Mutex::new(Vec::new()));
    let log = Arc::clone(&seen);
    let renamed = path.clone();
    *unpoison(s.hooks.before_fsync.lock()) = Some(Box::new(move |p: &Path| {
        unpoison(log.lock()).push((p.to_path_buf(), renamed.exists()));
        Ok(())
    }));
    {
        let ap = s.append_lock();
        let g = unpoison(s.shared.read()).sealed[0].clone();
        s.finish_crashed_seal(&ap, &g).unwrap();
    }
    assert_eq!(
        *unpoison(seen.lock()),
        [(path.clone(), true), (dir.path().to_path_buf(), false)],
    );
    assert!(dir.path().join(segment_name(1, SEALED_SUFFIX)).exists());
}

/// Adoption's fsync is what lets a store acknowledge a tail it did not write.
#[test]
fn adopting_an_unsynced_tail_fsyncs_the_segment() {
    let objs = test_objects(2);
    let dir = killed_writer(&objs);
    let s = Store::open(dir.path()).unwrap();
    let seen = Arc::new(Mutex::new(Vec::new()));
    let log = Arc::clone(&seen);
    *unpoison(s.hooks.before_fsync.lock()) = Some(Box::new(move |p: &Path| {
        unpoison(log.lock()).push(p.to_path_buf());
        Ok(())
    }));
    s.put(objs[0].key, &objs[0].data).unwrap();
    assert!(
        unpoison(seen.lock()).contains(&active_path(dir.path(), 1)),
        "a put was acknowledged on an unsynced record"
    );
}

/// Finishing a seal looks first: the file may be gone, or no seal after all, or somebody
/// may hold it.
#[test]
fn finish_crashed_seal_takes_nothing_on_trust() {
    let objs = test_objects(1);
    let (src, path) = crashed_seal_let_go(&objs);
    let view = Store::open(src.path()).unwrap();
    let g = unpoison(view.shared.read()).sealed[0].clone();

    // Held: somebody is finishing it.
    let holder = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(&path)
        .unwrap();
    assert!(super::active::try_lock(&holder).unwrap());
    {
        let ap = view.append_lock();
        view.finish_crashed_seal(&ap, &g).unwrap();
    }
    assert!(
        path.exists(),
        "a seal somebody holds was finished under them"
    );
    drop(holder);

    // Not a seal: the footer is gone, as when a new owner has started it over.
    let body_end = g.fv.body_len;
    let f = fs::OpenOptions::new().write(true).open(&path).unwrap();
    f.set_len(body_end).unwrap();
    drop(f);
    {
        let ap = view.append_lock();
        view.finish_crashed_seal(&ap, &g).unwrap();
    }
    assert!(path.exists(), "a segment with no footer was renamed");
    assert!(!src.path().join(segment_name(1, SEALED_SUFFIX)).exists());

    // Gone.
    fs::remove_file(&path).unwrap();
    let ap = view.append_lock();
    view.finish_crashed_seal(&ap, &g).unwrap();
}

/// A closing store fsyncs its segment again. After a failed fsync that one reports success
/// without having written anything, and a synced mark would then promise what is lost.
#[test]
fn a_poisoned_store_leaves_no_synced_mark_behind() {
    let objs = test_objects(1);
    let dir = TempDir::new().unwrap();
    let s = Store::open(dir.path()).unwrap();
    s.put_verified_deferred(objs[0].key, &objs[0].data).unwrap();
    let (before, _) = durable_mark(dir.path());
    *unpoison(s.hooks.before_fsync.lock()) =
        Some(Box::new(|_: &Path| Err(io::Error::other("boom"))));
    s.sync().unwrap_err();
    *unpoison(s.hooks.before_fsync.lock()) = None;
    let _ = s.close();
    let (after, _) = durable_mark(dir.path());
    assert_eq!(after, before, "a synced mark after a failed fsync");
}

/// A view can hold a crashed seal that another store has since finished.
#[test]
fn segments_follows_a_seal_another_store_finished() {
    let objs = test_objects(2);
    let (dir, _) = crashed_seal_let_go(&objs);
    let s = Store::open(dir.path()).unwrap(); // maps it under its active name
    let other = Store::open(dir.path()).unwrap();
    other.put_verified(objs[0].key, &objs[0].data).unwrap(); // finishes the seal
    assert!(dir.path().join(segment_name(1, SEALED_SUFFIX)).exists());
    let segs = s.segments().unwrap();
    assert_eq!(segs.len(), 1);
    assert_eq!(segs[0].id, 1);
}

/// Once the view knows the seal is finished, it is a sealed segment like any other: a
/// write relies on it rather than make another copy.
#[test]
fn a_finished_seal_counts_once_the_view_knows() {
    let objs = test_objects(2);
    let (dir, _) = crashed_seal_let_go(&objs);
    let s = Store::open(dir.path()).unwrap();
    let other = Store::open(dir.path()).unwrap();
    other.put_verified(objs[0].key, &objs[0].data).unwrap(); // finishes the seal
    s.refresh().unwrap();
    s.put(objs[1].key, &objs[1].data).unwrap();
    assert!(
        !own_copy(&s, &objs[1]),
        "a second copy, beside a sealed segment that holds it durably"
    );
}

/// The ordinary seal: its footer's fsync is what every record in the segment now rests on.
#[test]
fn a_failed_fsync_sealing_poisons_the_store() {
    let objs = test_objects(2);
    let dir = TempDir::new().unwrap();
    let s = Store::open(dir.path()).unwrap();
    s.put(objs[0].key, &objs[0].data).unwrap();
    *unpoison(s.hooks.before_fsync.lock()) = Some(Box::new(|_| {
        Err(io::Error::new(io::ErrorKind::PermissionDenied, "EIO"))
    }));
    {
        let mut ap = s.append_lock();
        s.seal_active(&mut ap).expect_err("the seal went through");
    }
    *unpoison(s.hooks.before_fsync.lock()) = None;
    let err = s.put(objs[1].key, &objs[1].data).unwrap_err();
    assert!(err.to_string().contains("EIO"), "{err}");
}
