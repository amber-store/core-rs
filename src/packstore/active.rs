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
    Store, unpoison,
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
        let mut cands: Vec<_> = ls.active.iter().collect();
        // The fullest first, so that one fills and seals before the next.
        cands.sort_by(|a, b| {
            b.1.size
                .cmp(&a.1.size)
                .then_with(|| a.1.path.cmp(&b.1.path))
        });
        for (id, sf) in cands {
            if self.adopt(ap, *id, &sf.path)? {
                return Ok(());
            }
        }
        self.create_active(ap, ls.max_id)
    }

    /// Tries to take the active segment at `path`. It reports false when
    /// somebody else holds it, when it is gone, or when it turned out to be a
    /// crashed seal, which is finished here and leaves nothing to append to
    /// (Go: `adopt`).
    pub(super) fn adopt(&self, ap: &mut AppendState, id: u64, path: &Path) -> Result<bool, Error> {
        let f = match OpenOptions::new().read(true).write(true).open(path) {
            Ok(f) => f,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(false),
            Err(e) => return Err(e.into()),
        };
        if !try_lock(&f)? || !is_file_at(&f, path) {
            return Ok(false);
        }
        // The segment is this store's now: only from here on may it be modified.
        let mut res = recover_segment(path)?;
        if res.sealed {
            // A crash between the footer's write and the rename: finish it.
            let sealed_path = sealed_path_of(path);
            fs::rename(path, &sealed_path)?;
            ap.dir_f.as_ref().ok_or(Error::Closed)?.sync_all()?;
            let _ = fs::remove_file(with_suffix(path, SIDECAR_SUFFIX)); // a sealed segment indexes itself
            let seg = SealedSegment::open(&sealed_path, id)?;
            drop(f);
            let mut sh = unpoison(self.shared.write());
            publish_sealed(&mut sh, Arc::new(seg));
            return Ok(false);
        }
        let header_len = MAGIC_HEADER.len() as u64;
        let size = if res.data_end < header_len as i64 {
            // The header never became durable, so nothing in the file was
            // ever acknowledged: start it over. Deliberate and silent.
            res.sidecar_end = 0;
            res.missing.clear();
            f.set_len(0)?;
            f.write_all_at(&MAGIC_HEADER, 0)?;
            header_len
        } else {
            f.set_len(res.data_end as u64)?;
            res.data_end as u64
        };
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
        ap.active = Some(ActiveWriter {
            seg,
            size,
            reserved: size,
            sc,
        });
        Ok(true)
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
                f.write_all_at(&MAGIC_HEADER, 0)?;
                f.sync_all()?;
                fs::rename(&tmp, &final_path)?;
                ap.dir_f.as_ref().ok_or(Error::Closed)?.sync_all()?;
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
                size: MAGIC_HEADER.len() as u64,
                reserved: MAGIC_HEADER.len() as u64,
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

    /// Seals every active segment that no writer holds. A small store's
    /// segments never fill, and the process that wrote them may be long gone:
    /// without this nothing in them could ever be collected. A segment a live
    /// writer holds is left alone; an active segment is never a victim.
    /// Called by `compact` under the append lock, after it sealed the store's
    /// own segment (Go: `sealIdleLocked`).
    pub(super) fn seal_idle(&self, ap: &mut AppendState) -> Result<(), Error> {
        if let Some(aw) = ap.active.take() {
            // Still owned, so empty: sealing left it alone. Let go of it for
            // the pass; it is on disk for whoever writes next.
            unlock(&aw.seg.f);
            let mut sh = unpoison(self.shared.write());
            sh.struct_epoch += 1;
            sh.active = None;
            // The segment is now neither this store's nor in its view of the
            // others', and whoever takes it next changes nothing in the
            // directory: the next lookup that misses has to list it.
            sh.dir_mtime = None;
        }
        let ls = list_segments(&self.dir)?;
        let mut ids: Vec<u64> = ls.active.keys().copied().collect();
        // The fullest first: the empty ones cannot be sealed.
        ids.sort_by(|a, b| {
            ls.active[b]
                .size
                .cmp(&ls.active[a].size)
                .then_with(|| a.cmp(b))
        });
        for id in ids {
            if !self.adopt(ap, id, &ls.active[&id].path)? {
                continue;
            }
            if let Err(e) = self.seal_active(ap) {
                if !e.is_capacity() {
                    self.set_failed(&e);
                }
                return Err(e);
            }
            if ap.active.is_some() {
                return Ok(()); // an empty one: the pass appends its survivors to it
            }
        }
        Ok(())
    }
}
