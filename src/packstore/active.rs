//! Owning an active segment (Go: `packstore/active.go`).
//!
//! A writer owns an active segment by holding an exclusive flock on its data
//! file, from the moment it takes the segment until close or the seal.
//! Nothing is locked at open, so a store that only reads never keeps a
//! segment from the next writer. At its first write a store adopts before it
//! creates: it takes the largest active segment nobody holds, and makes a new
//! one only when every one is taken. Serial writers therefore keep filling
//! one segment to the segment size, as a single process always did; the
//! number of active segments is bounded by the peak number of simultaneous
//! writers.

use std::collections::HashMap;
use std::ffi::OsString;
use std::fs::{self, File, OpenOptions};
use std::io;
use std::os::fd::AsRawFd;
use std::os::unix::fs::FileExt;
use std::path::Path;
use std::sync::{Arc, RwLock};

use super::footer::SealedSegment;
use super::recover_sidecar::{open_owned_sidecar, recover_segment};
use super::sidecar::{SIDECAR_SUFFIX, SidecarWriter};
use super::view::{
    FileIdent, ForeignActive, TMP_SUFFIX, drop_foreign, list_segments, publish_sealed,
    sealed_path_of, segment_name, with_suffix,
};
use super::{
    ACTIVE_SUFFIX, ActiveSegment, ActiveWriter, AppendState, Error, MAGIC_HEADER, SEALED_SUFFIX,
    Store, VERSION_BASE, header_at, is_header, unpoison,
};

/// Takes an exclusive flock without waiting (Go: `tryLock`).
pub(crate) fn try_lock(f: &File) -> io::Result<bool> {
    loop {
        // SAFETY: plain flock(2) on a valid open fd; no memory is involved.
        if unsafe { libc::flock(f.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0 {
            return Ok(true);
        }
        let e = io::Error::last_os_error();
        match e.raw_os_error() {
            Some(libc::EWOULDBLOCK) => return Ok(false),
            Some(libc::EINTR) => {}
            _ => return Err(e),
        }
    }
}

/// Lets go of a segment's flock at once. The file closes with its last
/// handle, which a reader that resolved a location a moment ago may still
/// hold (Go closes the file, which also unlocks it).
pub(crate) fn unlock(f: &File) {
    // SAFETY: plain flock(2) on a valid open fd; no memory is involved.
    unsafe { libc::flock(f.as_raw_fd(), libc::LOCK_UN) };
}

/// Reports whether `f` is still the file at `path`: between a listing and a
/// lock, a segment can be sealed (renamed) or reaped (Go: `isFileAt`).
pub(crate) fn is_file_at(f: &File, path: &Path) -> bool {
    match (f.metadata(), fs::metadata(path)) {
        (Ok(a), Ok(b)) => FileIdent::of(&a) == FileIdent::of(&b),
        _ => false,
    }
}

/// Go: `segmentExists`.
fn segment_exists(dir: &Path, id: u64) -> io::Result<bool> {
    for suffix in [ACTIVE_SUFFIX, SEALED_SUFFIX] {
        match fs::metadata(dir.join(segment_name(id, suffix))) {
            Ok(_) => return Ok(true),
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => return Err(e),
        }
    }
    Ok(false)
}

impl Store {
    /// Makes sure the store owns an active segment to append to. Called under
    /// the append lock (Go: `ensureActiveLocked`).
    pub(super) fn ensure_active(&self, ap: &mut AppendState) -> Result<(), Error> {
        if ap.active.is_some() {
            return Ok(());
        }
        let ls = list_segments(&self.dir)?;
        self.remove_stale_temporaries(&ls.tmp);
        // What a compact's own segment holds stands in for a victim's copy, because the
        // pass verified it. One that somebody left while it ran holds what nobody
        // verified, and may be a victim the pass is reading: a crashed seal. And lookups
        // read the newest id first, so a copy must not go under an id below a pack that
        // survives with a bad copy of the same key: it takes a new one.
        if ap.copying {
            return self.create_active(ap, ls.max_id);
        }
        let mut cands: Vec<_> = ls.active.iter().collect();
        // The fullest first, so that one fills and seals before the next.
        cands.sort_by(|a, b| {
            b.1.size
                .cmp(&a.1.size)
                .then_with(|| a.1.path.cmp(&b.1.path))
        });
        let min_version = ap.seg_version;
        for (id, sf) in cands {
            if self.adopt(ap, *id, &sf.path, min_version)? {
                return Ok(());
            }
        }
        self.create_active(ap, ls.max_id)
    }

    /// Tries to take the active segment at `path`. It reports false when
    /// somebody else holds it, when it is gone, when its format version is
    /// below `min_version`, or when it turned out to be a crashed seal,
    /// which is finished here and leaves nothing to append to (Go: `adopt`).
    pub(super) fn adopt(
        &self,
        ap: &mut AppendState,
        id: u64,
        path: &Path,
        min_version: u8,
    ) -> Result<bool, Error> {
        let f = match OpenOptions::new().read(true).write(true).open(path) {
            Ok(f) => f,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(false),
            Err(e) => return Err(e.into()),
        };
        if !try_lock(&f)? || !is_file_at(&f, path) {
            return Ok(false);
        }
        // A segment below the version this store writes at is left as it is,
        // for a writer that can use it. One whose header never arrived is
        // taken: it gets this store's.
        let mut header = [0u8; MAGIC_HEADER.len()];
        let has_header = match f.read_exact_at(&mut header, 0) {
            Ok(()) => is_header(&header),
            // Short is fine: the header never arrived.
            Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => false,
            Err(e) => return Err(e.into()),
        };
        if has_header && header[MAGIC_HEADER.len() - 1] < min_version {
            return Ok(false);
        }
        // The segment is this store's now: only from here on may it be modified.
        let mut res = recover_segment(path)?;
        if res.sealed {
            self.finish_seal(ap, id, path, f)?;
            return Ok(false);
        }
        let header_len = MAGIC_HEADER.len() as u64;
        if res.data_end >= header_len as i64 && !has_header {
            // Recovery found records behind something that is not a header
            // this release reads: it cannot have, and nothing may be
            // appended here.
            return Err(super::corrupt(format!(
                "{}: records behind an unreadable header",
                path.display()
            )));
        }
        let mut version = header[MAGIC_HEADER.len() - 1];
        let size = if res.data_end < header_len as i64 {
            // The header never became durable, so nothing in the file was
            // ever acknowledged: start it over. Deliberate and silent.
            res.sidecar_end = 0;
            res.missing.clear();
            version = ap.seg_version;
            f.set_len(0)?;
            f.write_all_at(&header_at(version), 0)?;
            header_len
        } else {
            f.set_len(res.data_end as u64)?;
            res.data_end as u64
        };
        // From here on this store acknowledges what its own index holds, on the
        // grounds that its own fsync covers it (`reliable_active`, `append_locked`).
        // A tail its last writer never synced is not covered by anything yet, and
        // this one fsync makes the grounds true. A writer that synced or closed
        // cleanly leaves nothing to do, so a process per command pays nothing.
        let tail = self.cfg.sync && size as i64 > res.durable.max(header_len as i64);
        if tail {
            self.fsyncs
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            if let Err(e) = self.fsync(&f, path) {
                self.set_failed(&e);
                return Err(e.into()); // dropping f lets go of its lock
            }
        }
        let sc = open_owned_sidecar(path, &res);
        let seg = Arc::new(ActiveSegment {
            id,
            path: path.to_path_buf(),
            f,
            index: RwLock::new(res.index),
        });
        {
            let mut sh = unpoison(self.shared.write());
            sh.struct_epoch += 1;
            drop_foreign(&mut sh, id);
            sh.active = Some(seg.clone());
        }
        let mut writer = ActiveWriter {
            seg,
            version,
            size,
            sc,
        };
        if tail {
            writer.sidecar_synced();
        }
        ap.active = Some(writer);
        Ok(true)
    }

    /// Finishes a seal whose writer crashed between the footer's write and the
    /// rename. The crash may also have come before the seal's fsync, and a sealed
    /// segment is trusted as durable from here on. The caller holds `f`'s lock,
    /// and this lets go of it (Go: `finishSeal`).
    fn finish_seal(&self, ap: &AppendState, id: u64, path: &Path, f: File) -> Result<(), Error> {
        if let Err(e) = self.fsync(&f, path) {
            self.set_failed(&e);
            return Err(e.into());
        }
        let sealed_path = sealed_path_of(path);
        fs::rename(path, &sealed_path)?;
        if let Err(e) = self.fsync(ap.dir_f.as_ref().ok_or(Error::Closed)?, &self.dir) {
            self.set_failed(&e);
            return Err(e.into());
        }
        let _ = fs::remove_file(with_suffix(path, SIDECAR_SUFFIX)); // a sealed segment indexes itself
        let seg = SealedSegment::open(&sealed_path, id)?;
        drop(f);
        let mut sh = unpoison(self.shared.write());
        publish_sealed(&mut sh, Arc::new(seg));
        Ok(())
    }

    /// Finishes the seal a view holds under its active name, unless somebody else
    /// holds it. The caller holds the append lock (Go: `finishCrashedSeal`).
    pub(super) fn finish_crashed_seal(
        &self,
        ap: &AppendState,
        g: &SealedSegment,
    ) -> Result<(), Error> {
        let f = match OpenOptions::new().read(true).write(true).open(&g.path) {
            Ok(f) => f,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(()),
            Err(e) => return Err(e.into()),
        };
        if !try_lock(&f)? || !is_file_at(&f, &g.path) {
            return Ok(());
        }
        if !recover_segment(&g.path)?.sealed {
            return Ok(());
        }
        self.finish_seal(ap, g.id, &g.path, f)
    }

    /// Makes a new active segment with an id above every one in the
    /// directory. The id is claimed by creating the segment's temporary name
    /// exclusively; the file is locked, checked, written and only then
    /// renamed to the name other stores look for, so nobody ever sees — or
    /// adopts — a segment that is not ready and owned (Go: `createActive`).
    fn create_active(&self, ap: &mut AppendState, max_id: u64) -> Result<(), Error> {
        // Wrapping, as Go's uint64 arithmetic does: a name with the highest id,
        // left behind or planted, must not panic a debug build's first write.
        let mut id = ap.next_id.max(max_id.wrapping_add(1));
        loop {
            let final_path = self.dir.join(segment_name(id, ACTIVE_SUFFIX));
            let tmp = with_suffix(&final_path, TMP_SUFFIX);
            let f = match OpenOptions::new()
                .read(true)
                .write(true)
                .create_new(true)
                .open(&tmp)
            {
                Ok(f) => f,
                Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {
                    id = id.wrapping_add(1); // another creator is at this id
                    continue;
                }
                Err(e) => return Err(e.into()),
            };
            // A store clearing away what crashes left behind may have taken
            // the file for such a leftover in the instant before this lock:
            // it holds the lock, or it has already removed the file and let
            // go. Either way the file is lost; claim the next id.
            if !try_lock(&f)? || !is_file_at(&f, &tmp) {
                id = id.wrapping_add(1);
                continue;
            }
            let abandon = |f: File| {
                drop(f);
                let _ = fs::remove_file(&tmp);
            };
            // The temporary name is free again as soon as its creator renames
            // it, so holding it does not prove the id is new: look.
            match segment_exists(&self.dir, id) {
                Ok(false) => {}
                Ok(true) => {
                    abandon(f);
                    id = id.wrapping_add(1);
                    continue;
                }
                Err(e) => {
                    abandon(f);
                    return Err(e.into());
                }
            }
            let init = (|| -> Result<(), Error> {
                f.write_all_at(&header_at(ap.seg_version), 0)?;
                self.fsync(&f, &tmp)?;
                fs::rename(&tmp, &final_path)?;
                self.fsync(ap.dir_f.as_ref().ok_or(Error::Closed)?, &self.dir)?;
                Ok(())
            })();
            if let Err(e) = init {
                // While the lock still holds: nobody may adopt a segment that
                // is on its way out.
                let _ = fs::remove_file(&final_path);
                abandon(f);
                return Err(e);
            }
            // None on failure: a segment works without one.
            let sc = SidecarWriter::create(&with_suffix(&final_path, SIDECAR_SUFFIX)).ok();
            let seg = Arc::new(ActiveSegment {
                id,
                path: final_path,
                f,
                index: RwLock::new(HashMap::new()),
            });
            ap.next_id = id.wrapping_add(1);
            {
                let mut sh = unpoison(self.shared.write());
                sh.struct_epoch += 1;
                sh.active = Some(seg.clone());
            }
            ap.active = Some(ActiveWriter {
                seg,
                version: ap.seg_version,
                size: MAGIC_HEADER.len() as u64,
                sc,
            });
            return Ok(());
        }
    }

    /// Deletes what crashed creations left behind. One that is locked is
    /// somebody's creation in progress (Go: `removeStaleTemporaries`).
    fn remove_stale_temporaries(&self, names: &[OsString]) {
        for name in names {
            let path = self.dir.join(name);
            let Ok(f) = OpenOptions::new().read(true).write(true).open(&path) else {
                continue;
            };
            if try_lock(&f).unwrap_or(false) && is_file_at(&f, &path) {
                let _ = fs::remove_file(&path);
            }
        }
    }

    /// Takes every active segment this store does not own, for an operation
    /// that is about to delete them. It fails, holding nothing, if one is
    /// held by a live writer, which would go on appending to a file that is
    /// gone (Go: `lockForeign`).
    pub(super) fn lock_foreign(&self, foreign: &[Arc<ForeignActive>]) -> Result<Vec<File>, Error> {
        let mut held = Vec::new();
        for fa in foreign {
            let f = match OpenOptions::new().read(true).write(true).open(&fa.path) {
                Ok(f) => f,
                Err(e) if e.kind() == io::ErrorKind::NotFound => continue,
                Err(e) => return Err(e.into()),
            };
            if !try_lock(&f)? {
                return Err(Error::Other(format!(
                    "packstore: segment {:016x} is being written to by another store",
                    fa.id
                )));
            }
            held.push(f);
        }
        Ok(held)
    }

    /// Lets go of the store's active segment as it is: it stays on disk,
    /// unsealed, for whoever writes next. Does nothing when the store owns
    /// none. Called under the append lock (Go: `releaseActiveLocked`).
    pub(super) fn release_active(&self, ap: &mut AppendState) {
        let Some(aw) = ap.active.take() else {
            return;
        };
        unlock(&aw.seg.f);
        let mut sh = unpoison(self.shared.write());
        sh.struct_epoch += 1;
        sh.active = None;
        // The segment is now neither this store's nor in its view of the
        // others', and whoever takes it next changes nothing in the
        // directory: the next lookup that misses has to list it.
        sh.dir_mtime = None;
    }

    /// Gives up the store's active segment so that the next append takes
    /// another: seals it, or, when nothing is in it yet, lets go of it.
    /// Called under the append lock (Go: `leaveActiveLocked`).
    pub(super) fn leave_active(&self, ap: &mut AppendState) -> Result<(), Error> {
        self.seal_active(ap)?;
        self.release_active(ap); // empty: sealing left it alone
        Ok(())
    }

    /// Seals every active segment that no writer holds, and lets go of those with
    /// nothing in them. A small store's segments never fill, and the process that wrote
    /// them may be long gone: without this nothing in them could ever be collected. A
    /// segment a live writer holds is left alone, and is no victim. Called by `compact`
    /// under the append lock, after it sealed the store's own segment (Go:
    /// `sealIdleLocked`).
    pub(super) fn seal_idle(&self, ap: &mut AppendState) -> Result<(), Error> {
        // Still owned, so empty: sealing left it alone. Let go of it for the
        // pass; it is on disk for whoever writes next.
        self.release_active(ap);
        let ls = list_segments(&self.dir)?;
        for (id, sf) in &ls.active {
            // Whatever its version: it is to be sealed.
            if self.adopt(ap, *id, &sf.path, VERSION_BASE)? {
                self.leave_active(ap)?;
            }
        }
        Ok(())
    }
}
