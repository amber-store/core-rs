use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use crate::amberpack::{REC_HEADER_SIZE, decode_payload, parse_record};
use crate::key::Key;

use super::active::try_lock;
use super::compact::{all_entries, foreign_copies};
use super::footer::{IndexEntry, SealedSegment, build_footer};
use super::verify::verify_object;
use super::view::publish_sealed;
use super::{
    AppendState, Error, MAGIC_HEADER, Store, VERSION_BASE, header_at, unpoison, version_for,
};

pub(super) fn valid_record(key: Key, bytes: &[u8]) -> bool {
    let Ok(record) = parse_record(bytes) else {
        return false;
    };
    if record.key != key || bytes.len() != REC_HEADER_SIZE + record.slen as usize {
        return false;
    }
    let Ok(data) = decode_payload(record.flags, record.ulen, &bytes[REC_HEADER_SIZE..]) else {
        return false;
    };
    verify_object(key, &data).is_ok()
}

/// How often a deferred put looks at the directory. An ingest cannot afford a listing per
/// object, and what appeared since is too new to have gone bad (Go: `deferredLookEvery`).
pub(super) const DEFERRED_LOOK_EVERY: Duration = Duration::from_secs(1);

fn held_bad_copy(key: Key, id: u64) -> Error {
    Error::Other(format!(
        "packstore: key {key}: the copy in segment {id:016x} is bad, and another store holds that segment"
    ))
}

/// Reports whether somebody holds the segment at `path`, for now. A file that is gone is
/// held by nobody (Go: `isHeld`).
fn is_held(path: &Path) -> std::io::Result<bool> {
    let f = match OpenOptions::new().read(true).write(true).open(path) {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(e) => return Err(e),
    };
    Ok(!try_lock(&f)?)
}

pub(super) fn indexed_record_valid(segment: &SealedSegment, key: Key, off: u64, slen: u32) -> bool {
    let Ok(start) = usize::try_from(off) else {
        return false;
    };
    if start < MAGIC_HEADER.len() {
        return false;
    }
    start
        .checked_add(REC_HEADER_SIZE + slen as usize)
        .filter(|end| *end <= segment.fv.body_len as usize)
        .and_then(|end| segment.mm.get(start..end))
        .is_some_and(|bytes| valid_record(key, bytes))
}

impl Store {
    /// Verify supplied content and repair corrupt copies before acknowledging it.
    /// Replacement keeps pack IDs and uses a synced rename, so old mappings
    /// remain valid and a restart sees either complete version.
    /// All indexed copies are checked, including copies hidden by newer packs.
    /// New and healthy active records follow the store's sync option.
    /// Repairs always sync replacement files and their directory.
    /// Unindexed damage and corrupt footers require separate recovery.
    /// An error can leave some copies repaired; retrying is safe.
    ///
    /// A bad copy in another store's active segment is reached by taking that segment
    /// over, and sealing it. One that another store holds, in an active segment or a
    /// crashed seal, cannot be repaired, and lookups may read it before any other copy,
    /// so that is an error: call again when that store has let go.
    pub fn put_verified(&self, key: Key, data: &[u8]) -> Result<(), Error> {
        self.put_verified_with(key, data, true)
    }

    /// [`Store::put_verified`] without the fsync of a new or healthy active
    /// record. The caller ends a run of these with [`Store::sync`], and must
    /// not treat the content as stored before that returns. Repairs still
    /// sync.
    ///
    /// It looks at the directory at most once a second, so it does not answer for a
    /// copy that appeared since. Before it repairs or refuses anything it looks.
    pub fn put_verified_deferred(&self, key: Key, data: &[u8]) -> Result<(), Error> {
        self.put_verified_with(key, data, false)
    }

    /// Takes over every active segment of somebody else's that holds a bad copy of `key`,
    /// and seals all but the last, which puts the copies where a repair can reach them.
    /// With `skipped`, it returns true instead of acting on a bad one: look first. The
    /// caller holds the append lock (Go: `takeBadForeignCopies`).
    pub(super) fn take_bad_foreign_copies(
        &self,
        ap: &mut AppendState,
        key: Key,
        skipped: bool,
    ) -> Result<bool, Error> {
        let sh = unpoison(self.shared.read());
        let copies = foreign_copies(&sh, key);
        drop(sh);
        for c in copies {
            if c.raw.as_ref().is_some_and(|raw| valid_record(key, raw)) {
                continue;
            }
            if skipped {
                return Ok(true);
            }
            // Before this store's own segment is given up for it: a caller that keeps
            // trying while the other store lives must not seal a pack per try.
            if is_held(&c.fa.path)? {
                return Err(held_bad_copy(key, c.fa.id));
            }
            self.leave_active(ap)?;
            // Whatever its version: it is to be sealed.
            if !self.adopt(ap, c.fa.id, &c.fa.path, VERSION_BASE)? {
                match fs::metadata(&c.fa.path) {
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                    _ => return Err(held_bad_copy(key, c.fa.id)),
                }
                // Sealed by whoever held it, or finished here as a crashed seal.
                self.refresh()?;
            }
        }
        Ok(false)
    }

    fn put_verified_with(&self, key: Key, data: &[u8], sync_now: bool) -> Result<(), Error> {
        verify_object(key, data).map_err(Error::Verify)?;
        let _write_token = self.begin_write_token()?;
        self.observe(key);
        let mut ap = self.append_lock();
        let listed = {
            let sh = unpoison(self.shared.read());
            if sh.closed {
                return Err(Error::Closed);
            }
            if let Some(msg) = &sh.failed {
                return Err(Error::Failed(msg.clone()));
            }
            sh.listed_at
        };
        // What is in the directory now, not what this view last saw: this call answers
        // for every copy, and appending takes over a segment without looking at what it
        // holds.
        let skipped = !sync_now && listed.elapsed().is_ok_and(|ago| ago < DEFERRED_LOOK_EVERY);
        if !skipped {
            self.refresh_after_miss(false)?;
        }
        if !self.put_verified_in_view(&mut ap, key, data, sync_now, skipped)? {
            return Ok(());
        }
        // What it found wrong is what this view remembers, and a repair next door moves no
        // generation. A repair works from the mapping, so one from an old mapping would
        // put back what that repair took away.
        self.refresh()?;
        self.put_verified_in_view(&mut ap, key, data, sync_now, false)?;
        Ok(())
    }

    /// `put_verified` over the view as it stands. With `skipped`, this call has not looked
    /// at the directory, and rather than rewrite or refuse anything it returns true: look
    /// first. Called under the append lock (Go: `putVerifiedInView`).
    fn put_verified_in_view(
        &self,
        ap: &mut AppendState,
        key: Key,
        data: &[u8],
        sync_now: bool,
        skipped: bool,
    ) -> Result<bool, Error> {
        if self.take_bad_foreign_copies(ap, key, skipped)? {
            return Ok(true);
        }
        // A seal whose writer crashed may never have been fsynced: finish it, so that
        // what it holds can be relied on, or repaired, like any other segment's.
        if self.cfg.sync {
            let crashed: Vec<_> = unpoison(self.shared.read())
                .sealed
                .iter()
                .filter(|g| g.crashed_seal() && g.has(key))
                .cloned()
                .collect();
            for g in crashed {
                self.finish_crashed_seal(ap, &g)?;
            }
        }
        let mut found = false;
        let mut damaged = false;
        {
            let sh = unpoison(self.shared.read());
            if let Some(active) = &sh.active {
                use std::os::unix::fs::FileExt;
                if let Some(loc) = unpoison(active.index.read()).get(&key).copied() {
                    let mut bytes = vec![0; REC_HEADER_SIZE + loc.slen as usize];
                    active.f.read_exact_at(&mut bytes, loc.off)?;
                    found = true;
                    damaged |= !valid_record(key, &bytes);
                }
            }
            for segment in &sh.sealed {
                let Some((off, slen)) = segment.fv.lookup(&segment.mm, key) else {
                    continue;
                };
                let bad = !indexed_record_valid(segment, key, off, slen);
                if self.cfg.sync && segment.crashed_seal() {
                    // Somebody else holds it (the finishing above): no copy to rely on
                    // yet, and not this store's to rewrite.
                    if bad && skipped {
                        return Ok(true);
                    }
                    if bad {
                        return Err(held_bad_copy(key, segment.id));
                    }
                    continue;
                }
                found = true;
                damaged |= bad;
            }
        }

        if !found {
            // Encode first: an object the callback rejects wrote nothing.
            let record = self.cfg.encode(key, data)?;
            if !sync_now {
                self.deferred
                    .store(true, std::sync::atomic::Ordering::SeqCst);
            }
            self.append_locked(ap, key, &record, sync_now)?;
            return Ok(false);
        }
        if !damaged {
            // A concurrent batch can publish a record before its final sync.
            if sync_now
                && self.cfg.sync
                && let Some(active) = ap.active.as_mut()
                && unpoison(active.seg.index.read()).contains_key(&key)
            {
                if let Err(error) = self.fsync(&active.seg.f, &active.seg.path) {
                    self.set_failed(&error);
                    return Err(error.into());
                }
                active.sidecar_synced();
            }
            return Ok(false);
        }
        if skipped {
            return Ok(true);
        }
        // Encode before anything changes: a replacement the callback makes
        // impossible fails the repair with the store as it was.
        let replacement = self.cfg.encode(key, data)?;
        // Seal from the live index before replacement. Reopening a corrupt
        // active tail would otherwise discard records after the damaged one.
        self.seal_active(ap)?;
        let segments = unpoison(self.shared.read()).sealed.clone();
        for segment in segments {
            let Some((off, slen)) = segment.fv.lookup(&segment.mm, key) else {
                continue;
            };
            if indexed_record_valid(&segment, key, off, slen) {
                continue;
            }
            let mut entries: Vec<_> = all_entries(&segment).collect();
            entries.sort_by_key(|entry| entry.off);
            let temporary = segment.path.with_extension("repair");
            // A previous crash may have left an unpublished file with this
            // ignored suffix.
            match fs::remove_file(&temporary) {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(error.into()),
            }
            let replace = (|| -> Result<(), Error> {
                let mut file = OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .open(&temporary)?;
                // The rewritten segment keeps its version, raised when the
                // replacement is a record that the old one may not hold.
                let version = segment.version().max(version_for(key, replacement[33]));
                file.write_all(&header_at(version))?;
                let mut offset = MAGIC_HEADER.len() as u64;
                let mut index = Vec::with_capacity(entries.len());
                for entry in &entries {
                    let bytes = if entry.k == key {
                        replacement.as_slice()
                    } else {
                        let start = usize::try_from(entry.off)
                            .map_err(|_| super::corrupt("repair offset overflow"))?;
                        if start < MAGIC_HEADER.len() {
                            return Err(super::corrupt("repair record overlaps segment header"));
                        }
                        let end = start
                            .checked_add(REC_HEADER_SIZE + entry.slen as usize)
                            .filter(|end| *end <= segment.fv.body_len as usize)
                            .ok_or_else(|| super::corrupt("repair record overflow"))?;
                        segment
                            .mm
                            .get(start..end)
                            .ok_or_else(|| super::corrupt("repair record is outside its segment"))?
                    };
                    file.write_all(bytes)?;
                    index.push(IndexEntry {
                        k: entry.k,
                        off: offset,
                        slen: (bytes.len() - REC_HEADER_SIZE) as u32,
                    });
                    offset += bytes.len() as u64;
                }
                file.write_all(&build_footer(offset, &index)?)?;
                self.fsync(&file, &temporary)?;
                let mut ready = SealedSegment::open(&temporary, segment.id)?;
                ready.path = segment.path.clone();
                fs::rename(&temporary, &segment.path)?;
                if let Err(error) = self.fsync(ap.dir_f.as_ref().ok_or(Error::Closed)?, &self.dir) {
                    self.set_failed(&error);
                    return Err(error.into());
                }
                // The replacement takes the segment's place by id. Readers
                // that hold the old mapping keep it until they let go.
                let mut shared = unpoison(self.shared.write());
                publish_sealed(&mut shared, Arc::new(ready));
                Ok(())
            })();
            if replace.is_err() {
                let _ = fs::remove_file(&temporary);
            }
            replace?;
        }
        Ok(false)
    }
}
