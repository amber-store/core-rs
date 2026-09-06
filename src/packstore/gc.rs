//! Format-neutral views of sealed segments: segment listing, index scans,
//! raw record reads and re-appends, removal, and the in-flight write horizon.
//! The mark-and-sweep collector (the `gc` module) uses [`Store::segments`]
//! for its status report; the rest stays as the store's generic GC surface
//! (Go: `packstore/gc.go`).

use std::collections::HashMap;
use std::fs;
use std::sync::Arc;
use std::time::{Instant, SystemTime};

use crate::amberpack::{REC_HEADER_SIZE, parse_record};
use crate::key::{self, Key};

use super::footer::{INDEX_ENTRY_SIZE, SealedSegment};
use super::prepare::check_record;
use super::{Error, MAGIC_HEADER, Store, corrupt, unpoison};

/// In-flight exported-write starts (Go: the `writes map[*writeToken]time.Time`
/// and `writesMu` pair; the Rust port keys the map by a counter id instead of
/// a token pointer, and records [`Instant`]s — see
/// port-notes/packstore-gc.md).
pub(super) struct Writes {
    next: u64,
    inflight: HashMap<u64, Instant>,
}

impl Writes {
    pub(super) fn new() -> Writes {
        Writes {
            next: 0,
            inflight: HashMap::new(),
        }
    }
}

/// Marks one in-flight exported write call for the GC horizon; dropping it
/// deregisters the write, covering every return path like Go's deferred
/// `endWrite` (Go: `writeToken`).
pub(super) struct WriteToken<'a> {
    store: &'a Store,
    id: u64,
}

impl Drop for WriteToken<'_> {
    fn drop(&mut self) {
        unpoison(self.store.writes.lock()).inflight.remove(&self.id);
    }
}

/// Describes one sealed segment (Go: `SegmentInfo`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SegmentInfo {
    pub id: u64,
    /// The `.seg` file's mtime: the footer write is the file's last write
    /// (Go: `Sealed`).
    pub sealed: SystemTime,
    /// Record bytes: body length minus the header magic (Go: `Body`, an
    /// `int64`; `u64` here — the parsed footer guarantees the subtraction
    /// cannot underflow).
    pub body: u64,
    pub keys: u64,
}

impl Store {
    /// Registers one in-flight exported write for the GC horizon (Go:
    /// `beginWrite`; the paired `endWrite` is the returned token's `Drop`).
    pub(super) fn begin_write(&self) -> WriteToken<'_> {
        let mut w = unpoison(self.writes.lock());
        let id = w.next;
        w.next += 1;
        w.inflight.insert(id, Instant::now());
        WriteToken { store: self, id }
    }

    /// Returns the start time of the oldest [`Store::put`],
    /// [`Store::write_batch`] or [`Store::write_parallel`] still in progress,
    /// or `None` when no exported write is in flight. The GC horizon never
    /// passes it (Go: `OldestInflightWrite`, which returns a wall-clock
    /// `time.Time`; the port returns a monotonic [`Instant`] — see
    /// port-notes/packstore-gc.md).
    pub fn oldest_inflight_write(&self) -> Option<Instant> {
        unpoison(self.writes.lock())
            .inflight
            .values()
            .min()
            .copied()
    }

    /// Lists the sealed segments, ascending id. The active segment is not
    /// listed; it is never a reaping victim (Go: `Segments`).
    pub fn segments(&self) -> Result<Vec<SegmentInfo>, Error> {
        let sh = unpoison(self.shared.read());
        if sh.closed {
            return Err(Error::Closed);
        }
        let mut out = Vec::with_capacity(sh.sealed.len());
        for g in &sh.sealed {
            let sealed = fs::metadata(&g.path)?.modified()?;
            out.push(SegmentInfo {
                id: g.id,
                sealed,
                body: g.fv.body_len - MAGIC_HEADER.len() as u64,
                keys: g.fv.key_count,
            });
        }
        Ok(out)
    }

    /// Finds sealed segment `id` and returns an owning handle: the `Arc`
    /// keeps the mmap alive past a concurrent [`Store::remove`] or
    /// [`Store::compact`], so no scrub registration is needed (Go:
    /// `pinSegment` + `beginScrub`/`endScrub`; see
    /// port-notes/packstore-gc.md).
    fn pin_segment(&self, id: u64) -> Result<Arc<SealedSegment>, Error> {
        let sh = unpoison(self.shared.read());
        if sh.closed {
            return Err(Error::Closed);
        }
        for g in &sh.sealed {
            if g.id == id {
                return Ok(g.clone());
            }
        }
        Err(Error::UnknownSegment)
    }

    /// Walks segment `id`'s footer index in index order (fanout on the key's
    /// last byte, then full key), calling `f` with each entry's key, record
    /// offset and stored payload length. No pack body is read (Go:
    /// `ScanIndex`).
    pub fn scan_index(&self, id: u64, mut f: impl FnMut(Key, u64, u32)) -> Result<(), Error> {
        let seg = self.pin_segment(id)?;
        let fv = &seg.fv;
        let entries = &seg.mm[fv.entries_off..fv.entries_off + fv.entries_len];
        for row in entries.as_chunks::<INDEX_ENTRY_SIZE>().0 {
            let mut kb = [0u8; key::SIZE];
            kb.copy_from_slice(&row[..key::SIZE]);
            let off = u64::from_be_bytes([
                row[32], row[33], row[34], row[35], row[36], row[37], row[38], row[39],
            ]);
            f(Key(kb), off, super::be_u32(row, 40));
        }
        Ok(())
    }

    /// Returns the raw record at `off` in segment `id`, CRC-checked. The
    /// returned bytes are caller-owned and re-appendable verbatim (Go:
    /// `Record`).
    pub fn record(&self, id: u64, off: u64) -> Result<Vec<u8>, Error> {
        let seg = self.pin_segment(id)?;
        let body = seg.fv.body_len;
        // Overflow-safe: off may come from a crafted index; off +
        // REC_HEADER_SIZE could wrap, so the bound stays in subtraction form
        // (same shape as the sealed-segment readers in footer.rs).
        if off < MAGIC_HEADER.len() as u64 || off > body || REC_HEADER_SIZE as u64 > body - off {
            return Err(corrupt(format!(
                "{}: record offset {off} out of range",
                seg.path.display()
            )));
        }
        let rec =
            parse_record(&seg.mm[off as usize..body as usize]).map_err(|e| Error::Context {
                msg: format!("{}: offset {off}", seg.path.display()),
                source: Box::new(Error::Pack(e)),
            })?;
        let end = off + REC_HEADER_SIZE as u64 + u64::from(rec.slen);
        Ok(seg.mm[off as usize..end as usize].to_vec())
    }

    /// Reports whether `k` is stored anywhere but segment `id`: the active
    /// segment or any other sealed segment. Reaping uses it to skip records
    /// that already survived (Go: `HasOutside`).
    pub fn has_outside(&self, id: u64, k: Key) -> Result<bool, Error> {
        let sh = unpoison(self.shared.read());
        if sh.closed {
            return Err(Error::Closed);
        }
        if let Some(a) = &sh.active
            && unpoison(a.index.read()).contains_key(&k)
        {
            return Ok(true);
        }
        for g in sh.sealed.iter().rev() {
            if g.id == id {
                continue;
            }
            if g.has(k) {
                return Ok(true);
            }
        }
        Ok(false)
    }

    /// Re-appends an already-encoded record through the normal append path:
    /// no decode, no re-encode, no fsync — callers batch appends and call
    /// [`Store::sync`]. `raw` must be exactly one record, CRC-valid, keyed
    /// `k`: it gets the write paths' record check (the one an [`Object`](super::Object)
    /// offered with its `record` set gets), so an empty or otherwise
    /// non-record slice is rejected as corrupt rather than stored.
    /// Re-appending a key already in the active index is a silent no-op
    /// (the append path's existing dedup) (Go: `AppendRecord`).
    pub fn append_record(&self, k: Key, raw: &[u8]) -> Result<(), Error> {
        check_record(k, raw, false)?;
        self.append(k, raw, false)
    }

    /// Fsyncs the active segment, making every append so far durable —
    /// [`Store::append_record`] batches end with one `sync` (Go: `Sync`).
    pub fn sync(&self) -> Result<(), Error> {
        self.sync_active()
    }

    /// Drops sealed segment `id`: out of the probe list under the write
    /// locks (draining in-flight reads, like a seal), then unlink and a
    /// directory fsync. The caller ensures every live record was copied out
    /// first. In-flight scans and reads keep the mmap alive through their
    /// `Arc` handles, so unlike Go there is no scrub wait before munmap; the
    /// mapping is released when the last handle drops (Go: `Remove` +
    /// `waitScrubs`; see port-notes/packstore-gc.md).
    pub fn remove(&self, id: u64) -> Result<(), Error> {
        // Lock order: append lock before shared lock, like every writer. The
        // append guard also protects the directory handle for the fsync.
        let ap = self.append_lock();
        let seg = {
            let mut sh = unpoison(self.shared.write());
            if sh.closed {
                return Err(Error::Closed);
            }
            let Some(idx) = sh.sealed.iter().position(|g| g.id == id) else {
                return Err(Error::UnknownSegment);
            };
            // Concurrent readers hold Arc clones, never the Vec itself, so
            // removing in place under the write lock is safe (Go rebuilds a
            // fresh slice because its readers may hold the backing array).
            sh.sealed.remove(idx)
        };
        // Go collects a firstErr across seg.close() (munmap), unlink, and
        // the dir fsync, attempting all three; the Rust munmap happens on
        // the last Arc drop and cannot report an error.
        let mut first_err: Option<Error> = None;
        if let Err(e) = fs::remove_file(&seg.path) {
            first_err.get_or_insert(e.into());
        }
        if self.cfg.sync
            && let Some(dir_f) = ap.dir_f.as_ref()
            && let Err(e) = dir_f.sync_all()
        {
            first_err.get_or_insert(e.into());
        }
        first_err.map_or(Ok(()), Err)
    }
}
