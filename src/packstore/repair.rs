use std::fs::{self, OpenOptions};
use std::io::Write;
use std::sync::Arc;

use crate::amberpack::{REC_HEADER_SIZE, decode_payload, encode_record, parse_record};
use crate::key::Key;

use super::compact::all_entries;
use super::footer::{IndexEntry, SealedSegment, build_footer};
use super::verify::verify_object;
use super::view::publish_sealed;
use super::{Error, MAGIC_HEADER, Store, unpoison};

fn valid_record(key: Key, bytes: &[u8]) -> bool {
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

fn indexed_record_valid(segment: &SealedSegment, key: Key, off: u64, slen: u32) -> bool {
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
    pub fn put_verified(&self, key: Key, data: &[u8]) -> Result<(), Error> {
        verify_object(key, data).map_err(|msg| Error::Corrupt { msg, verify: true })?;
        let _write_token = self.begin_write_token()?;
        self.observe(key);
        let mut ap = self.append_lock();
        {
            let sh = unpoison(self.shared.read());
            if sh.closed {
                return Err(Error::Closed);
            }
            if let Some(msg) = &sh.failed {
                return Err(Error::Failed(msg.clone()));
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
                if let Some((off, slen)) = segment.fv.lookup(&segment.mm, key) {
                    found = true;
                    damaged |= !indexed_record_valid(segment, key, off, slen);
                }
            }
        }

        if !found {
            let record = encode_record(key, data).map_err(Error::Pack)?;
            return self.append_locked(&mut ap, key, &record, true);
        }
        if !damaged {
            // A concurrent batch can publish a record before its final sync.
            if self.cfg.sync
                && let Some(active) = ap.active.as_mut()
                && unpoison(active.seg.index.read()).contains_key(&key)
            {
                if let Err(error) = active.seg.f.sync_all() {
                    self.set_failed(&error);
                    return Err(error.into());
                }
                active.sidecar_synced();
            }
            return Ok(());
        }
        // Seal from the live index before replacement. Reopening a corrupt
        // active tail would otherwise discard records after the damaged one.
        if let Err(error) = self.seal_active(&mut ap) {
            self.set_failed(&error);
            return Err(error);
        }
        let replacement = encode_record(key, data).map_err(Error::Pack)?;
        let segments = unpoison(self.shared.read()).sealed.clone();
        let mut repaired = false;
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
                file.write_all(&MAGIC_HEADER)?;
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
                file.sync_all()?;
                let mut ready = SealedSegment::open(&temporary, segment.id)?;
                ready.path = segment.path.clone();
                fs::rename(&temporary, &segment.path)?;
                if let Err(error) = ap.dir_f.as_ref().ok_or(Error::Closed)?.sync_all() {
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
            repaired = true;
        }
        if !repaired {
            return Err(super::corrupt("no indexed record could be repaired"));
        }
        Ok(())
    }
}
