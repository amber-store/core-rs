//! The mark-and-sweep GC surface: [`Store::liveness`] (the dry run) and
//! [`Store::compact`] (the sweep), driven by the collector in the `gc` module
//! against a [`super::MarkSet`] built from the references' closures (Go:
//! `packstore/compact.go`, ported from Mic92's bitmap GC; see
//! `architecture/mark-sweep-gc.md` and `specs/gc.qnt` in the Go repo).

use std::collections::HashSet;
use std::fs;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::thread;
use std::time::SystemTime;

use crate::amberpack::{REC_HEADER_SIZE, decode_payload, parse_record};
use crate::key::{self, Key};

use super::footer::{INDEX_ENTRY_SIZE, IndexEntry, SealedSegment};
use super::verify::verify_object;
use super::{AppendState, Error, MAGIC_HEADER, Store, be_u32, corrupt, unpoison};

/// One segment's liveness breakdown. Byte counts cover record bytes only,
/// not footer overhead (Go: `SegmentLiveness`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SegmentLiveness {
    pub id: u64,
    pub sealed: bool,
    pub live_keys: usize,
    pub dead_keys: usize,
    pub live_bytes: u64,
    pub dead_bytes: u64,
}

impl SegmentLiveness {
    /// Classifies one record of `slen` stored bytes (Go:
    /// `(*SegmentLiveness).add`).
    fn add(&mut self, is_live: bool, slen: u32) {
        let n = REC_HEADER_SIZE as u64 + u64::from(slen);
        if is_live {
            self.live_keys += 1;
            self.live_bytes += n;
        } else {
            self.dead_keys += 1;
            self.dead_bytes += n;
        }
    }
}

/// Tunes one [`Store::compact`] pass (Go: `CompactOpts`).
pub struct CompactOpts {
    /// The selection line: a sealed segment whose dead bytes reach this
    /// fraction of its record bytes is rewritten (Go: `MinDeadRatio`).
    pub min_dead_ratio: f64,
    /// When set, excludes segments sealed at or after it — the gc grace
    /// period. `None` means every sealed segment is eligible (Go: `Horizon`,
    /// whose zero value maps to `None`).
    pub horizon: Option<SystemTime>,
    /// When set, called with each copied record's size; the collector uses
    /// it to cap copy bandwidth (Go: `Pace`).
    pub pace: Option<Box<dyn FnMut(usize) + Send>>,
    /// Caps the live record bytes this pass copies. Victim selection skips a
    /// segment whose live bytes exceed what is left and goes on to smaller
    /// ones. Record bytes only: footers and allocation overhead sit outside
    /// it. At `0` fully dead segments are still reclaimed, since those need no
    /// copy. The default, `u64::MAX`, is unbounded (Rust-only).
    pub max_copy_bytes: u64,
}

impl Default for CompactOpts {
    fn default() -> CompactOpts {
        CompactOpts {
            min_dead_ratio: 0.0,
            horizon: None,
            pace: None,
            max_copy_bytes: u64::MAX,
        }
    }
}

impl std::fmt::Debug for CompactOpts {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CompactOpts")
            .field("min_dead_ratio", &self.min_dead_ratio)
            .field("horizon", &self.horizon)
            .field("pace", &self.pace.is_some())
            .finish()
    }
}

/// Summarizes one [`Store::compact`] pass (Go: `CompactStats`).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CompactStats {
    /// Sealed segments considered.
    pub segments_scanned: usize,
    pub segments_compacted: usize,
    /// Compacted segment ids, ascending.
    pub victims: Vec<u64>,
    pub records_copied: usize,
    pub bytes_copied: u64,
    /// Victim file sizes, footers included.
    pub bytes_freed: u64,
}

/// Decodes every 44-byte index row of a sealed segment, in index order (Go:
/// `footerView.allEntries`; a plain iterator rather than `iter.Seq`).
pub(super) fn all_entries(g: &SealedSegment) -> impl Iterator<Item = IndexEntry> + '_ {
    let entries = &g.mm[g.fv.entries_off..g.fv.entries_off + g.fv.entries_len];
    entries.as_chunks::<INDEX_ENTRY_SIZE>().0.iter().map(|row| {
        let mut kb = [0u8; key::SIZE];
        kb.copy_from_slice(&row[..key::SIZE]);
        IndexEntry {
            k: Key(kb),
            off: u64::from_be_bytes([
                row[32], row[33], row[34], row[35], row[36], row[37], row[38], row[39],
            ]),
            slen: be_u32(row, 40),
        }
    })
}

/// Classifies every index entry of one sealed segment by `live` (Go:
/// `sealedSegment.liveness`).
fn segment_liveness(g: &SealedSegment, live: &dyn Fn(Key) -> bool) -> SegmentLiveness {
    let mut info = SegmentLiveness {
        id: g.id,
        sealed: true,
        ..SegmentLiveness::default()
    };
    for e in all_entries(g) {
        info.add(live(e.k), e.slen);
    }
    info
}

/// One record travelling the verify pipeline. The record bytes are a
/// zero-copy view into the victim's mmap, kept alive by the `Arc` (Go:
/// `copyRec`, whose `rec` slice borrows the mmap).
struct CopyRec {
    g: Arc<SealedSegment>,
    e: IndexEntry,
}

impl CopyRec {
    fn rec(&self) -> &[u8] {
        let start = self.e.off as usize;
        &self.g.mm[start..start + REC_HEADER_SIZE + self.e.slen as usize]
    }
}

/// First-error slot + cancellation flag for the copy pipeline (Go: errgroup +
/// `context.WithCancel`; the same shape as parallel.rs's `Run`).
struct Pipe {
    cancel: AtomicBool,
    first_err: Mutex<Option<Error>>,
}

impl Pipe {
    fn fail(&self, e: Error) {
        let mut slot = unpoison(self.first_err.lock());
        if slot.is_none() {
            *slot = Some(e);
        }
        drop(slot);
        self.cancel.store(true, Ordering::Relaxed);
    }

    fn canceled(&self) -> bool {
        self.cancel.load(Ordering::Relaxed)
    }
}

impl Store {
    /// Classifies every record by `live(k)` and reports per segment,
    /// ascending by id with the active segment last: the GC dry run (Go:
    /// `Liveness`).
    pub fn liveness(&self, live: impl Fn(Key) -> bool) -> Result<Vec<SegmentLiveness>, Error> {
        let sh = unpoison(self.shared.read());
        if sh.closed {
            return Err(Error::Closed);
        }
        let mut report = Vec::new();
        for g in &sh.sealed {
            report.push(segment_liveness(g, &live));
        }
        if let Some(a) = &sh.active {
            let mut info = SegmentLiveness {
                id: a.id,
                ..SegmentLiveness::default()
            };
            for (k, loc) in unpoison(a.index.read()).iter() {
                info.add(live(*k), loc.slen);
            }
            report.push(info);
        }
        for fa in &sh.foreign {
            // Other writers' active segments: never victims either.
            let mut info = SegmentLiveness {
                id: fa.id,
                ..SegmentLiveness::default()
            };
            for (k, loc) in unpoison(fa.state.read()).scan.index.iter() {
                info.add(live(*k), loc.slen);
            }
            report.push(info);
        }
        Ok(report)
    }

    /// Seals the active segment, rewrites eligible sealed segments whose dead
    /// ratio reaches `opts.min_dead_ratio` (re-verifying live records while
    /// copying), and deletes victims only after the copies are durable. The
    /// caller must guarantee no ingest or reference publication overlaps
    /// (`specs/gc.qnt`). A grey set captured since [`Store::begin_barrier`]
    /// is consumed — unconditionally, even if the pass then errors or selects
    /// no victims — and kept alongside `live` (Go: `Compact`).
    pub fn compact<L>(&self, live: L, opts: CompactOpts) -> Result<CompactStats, Error>
    where
        L: Fn(Key) -> bool + Sync,
    {
        let CompactOpts {
            min_dead_ratio,
            horizon,
            mut pace,
            max_copy_bytes,
        } = opts;
        // Other stores' writers wait for the whole pass and look at the
        // directory again afterwards (gate.rs); inside a collector's
        // begin_sweep the gate is held already. This store's own writes wait
        // too: a duplicate check must not race the removal of a segment, and
        // those that bypass the collector are held by nothing else.
        let _sweep = self.sweep_gate()?;
        let _pause = self.pause_local_writers();
        // The append lock is held for the entire pass: writers queue behind
        // the sweep (Go: appendMu for the whole call).
        let mut ap = self.append_lock();

        // Consume the barrier before anything can fail. Go wraps `live` only
        // when the set is non-empty; an empty or absent set greys nothing,
        // so the unconditional wrapper is equivalent.
        let grey = self.take_grey();
        let live = move |k: Key| grey.as_ref().is_some_and(|g| g.contains(&k)) || live(k);

        let mut stats = CompactStats::default();
        {
            let sh = unpoison(self.shared.read());
            if sh.closed {
                return Err(Error::Closed);
            }
            if let Some(msg) = &sh.failed {
                return Err(Error::Failed(msg.clone()));
            }
        }
        // Sealing writes a footer, which grows the store. A zero budget asks
        // for no growth, and must still reach the dead segments it can
        // reclaim without copying. seal_idle is skipped with it: it lets go
        // of this store's segment on the premise that the seal emptied it,
        // then adopts and seals whatever it finds, this one included.
        if max_copy_bytes > 0 {
            if let Err(e) = self.seal_active(&mut ap) {
                self.set_failed(&e);
                return Err(e);
            }
            self.seal_idle(&mut ap)?;
        }

        let victims =
            self.select_victims(&live, horizon, min_dead_ratio, max_copy_bytes, &mut stats)?;
        if victims.is_empty() {
            return Ok(stats);
        }
        // On error the victims stay untouched on disk: a failed compaction
        // must leave the victim files in place.
        self.copy_live(&mut ap, &victims, &live, &mut pace, &mut stats)?;
        // One fsync makes the whole copy batch durable before any unlink. If
        // copies rotated the active segment away, the seal already fsynced;
        // a `None` active skips.
        if self.cfg.sync
            && let Some(aw) = ap.active.as_mut()
        {
            if let Err(e) = aw.seg.f.sync_all() {
                let e: Error = e.into();
                self.set_failed(&e);
                return Err(e);
            }
            aw.sidecar_synced();
        }
        self.remove_victims(&mut ap, &victims, &mut stats)?;
        Ok(stats)
    }

    /// Snapshots the sealed segments and picks the rewrite victims: past the
    /// horizon (file mtime strictly before it), with at least one dead key,
    /// and with dead bytes at or above the ratio line (Go: `selectVictims`).
    /// A segment whose live bytes exceed `copy_left` is skipped; smaller ones
    /// after it are still considered.
    fn select_victims(
        &self,
        live: &(impl Fn(Key) -> bool + Sync),
        horizon: Option<SystemTime>,
        min_dead_ratio: f64,
        mut copy_left: u64,
        stats: &mut CompactStats,
    ) -> Result<Vec<Arc<SealedSegment>>, Error> {
        let segs: Vec<Arc<SealedSegment>> = unpoison(self.shared.read()).sealed.clone();
        stats.segments_scanned = segs.len();

        let mut victims = Vec::new();
        for g in segs {
            if let Some(h) = horizon {
                let mtime = fs::metadata(&g.path)?.modified()?;
                if mtime >= h {
                    continue; // eligible only when sealed strictly before the horizon
                }
            }
            let info = segment_liveness(&g, live);
            let total = info.live_bytes + info.dead_bytes;
            if info.dead_keys > 0 && info.dead_bytes as f64 >= min_dead_ratio * total as f64 {
                if info.live_bytes > copy_left {
                    continue;
                }
                copy_left -= info.live_bytes;
                stats.victims.push(g.id);
                victims.push(g);
            }
        }
        Ok(victims)
    }

    /// Appends every live victim record that has no copy in a surviving
    /// segment. Verification (decompress + re-hash) dominates, so workers
    /// verify in parallel while the calling thread — which holds the append
    /// lock — appends whatever is ready (Go: `copyLive`).
    fn copy_live(
        &self,
        ap: &mut AppendState,
        victims: &[Arc<SealedSegment>],
        live: &(impl Fn(Key) -> bool + Sync),
        pace: &mut Option<Box<dyn FnMut(usize) + Send>>,
        stats: &mut CompactStats,
    ) -> Result<(), Error> {
        let victim_ids: HashSet<u64> = victims.iter().map(|g| g.id).collect();
        // Victims are still in `shared.sealed` during the copy — reads keep
        // working — so survivor probes must skip them by id.
        let survivor_has = |k: Key| -> bool {
            let sh = unpoison(self.shared.read());
            // Not a copy a live writer has yet to sync: the victim's may be
            // the durable one.
            if Store::reliable_active(&sh, self.cfg.sync, k) {
                return true;
            }
            sh.sealed
                .iter()
                .any(|g| !victim_ids.contains(&g.id) && g.has(k))
        };

        let workers = thread::available_parallelism().map_or(1, |n| n.get());
        let pipe = Pipe {
            cancel: AtomicBool::new(false),
            first_err: Mutex::new(None),
        };
        let (cands_tx, cands_rx) = mpsc::sync_channel::<CopyRec>(4 * workers);
        let cands_rx = Mutex::new(cands_rx);
        let (verified_tx, verified_rx) = mpsc::sync_channel::<CopyRec>(4 * workers);

        let mut append_err: Option<Error> = None;
        thread::scope(|s| {
            let pipe = &pipe;
            let cands_rx = &cands_rx;
            let survivor_has = &survivor_has;
            // Producer: walk the victims' indexes in order, bounds-check each
            // entry, and feed zero-copy record views to the verifiers.
            s.spawn(move || {
                let tx = cands_tx; // moved in; dropping it closes the channel
                for g in victims {
                    let body_len = g.fv.body_len;
                    for e in all_entries(g) {
                        if pipe.canceled() {
                            return;
                        }
                        if !live(e.k) || survivor_has(e.k) {
                            continue;
                        }
                        // Overflow-safe, subtraction form: e.off came from
                        // the on-disk index and may be crafted.
                        if e.off < MAGIC_HEADER.len() as u64
                            || e.off > body_len
                            || REC_HEADER_SIZE as u64 + u64::from(e.slen) > body_len - e.off
                        {
                            pipe.fail(corrupt(format!(
                                "{}: index entry out of bounds",
                                g.path.display()
                            )));
                            return;
                        }
                        if tx.send(CopyRec { g: g.clone(), e }).is_err() {
                            return;
                        }
                    }
                }
            });
            // Verifiers: full record scrub (framing CRC, key match, payload
            // rehash) in parallel. On failure they record the error and keep
            // draining so the producer can never block on a full channel.
            for _ in 0..workers {
                let verified_tx = verified_tx.clone();
                s.spawn(move || {
                    loop {
                        let recv = unpoison(cands_rx.lock()).recv();
                        let Ok(c) = recv else { return };
                        if pipe.canceled() {
                            continue; // drain
                        }
                        if let Err(e) = verify_record(&c.g, c.e, c.rec()) {
                            pipe.fail(e);
                            continue;
                        }
                        if verified_tx.send(c).is_err() {
                            return;
                        }
                    }
                });
            }
            // The workers' clones keep the channel open; when the last one
            // exits, `verified_rx` closes and the append loop below ends
            // (Go: the goroutine closing `verified` after `pipe.Wait()`).
            drop(verified_tx);

            // Single appender: the calling thread, already holding the
            // append lock, so records land through `append_locked`. Appends
            // never fsync — rotation can seal mid-copy, which fsyncs that
            // segment; `compact` issues the batch fsync afterwards.
            for c in verified_rx.iter() {
                if append_err.is_some() {
                    continue; // drain
                }
                if let Err(e) = self.append_locked(ap, c.e.k, c.rec(), false) {
                    pipe.cancel.store(true, Ordering::Relaxed);
                    append_err = Some(e);
                    continue;
                }
                stats.records_copied += 1;
                stats.bytes_copied += c.rec().len() as u64;
                if let Some(p) = pace.as_mut() {
                    p(c.rec().len());
                }
            }
        });

        // Append errors first, then the pipeline's first error (Go:
        // `cmp.Or(appendErr, pipe.Wait())`).
        if let Some(e) = append_err {
            return Err(e);
        }
        match unpoison(pipe.first_err.into_inner()) {
            Some(e) => Err(e),
            None => Ok(()),
        }
    }

    /// Detaches the victims from the probe list, then unlinks their files
    /// and fsyncs the directory once. In-flight scans and reads keep the
    /// victims' mmaps alive through their `Arc` handles; the mappings are
    /// released when the last handle drops, so unlike Go there is no scrub
    /// wait before munmap (Go: `removeVictims` + `waitScrubs`; see
    /// port-notes/packstore-gc.md).
    fn remove_victims(
        &self,
        ap: &mut AppendState,
        victims: &[Arc<SealedSegment>],
        stats: &mut CompactStats,
    ) -> Result<(), Error> {
        let victim_ids: HashSet<u64> = victims.iter().map(|g| g.id).collect();
        // A lookup that finds nothing lists the directory (view.rs). Between
        // the victims leaving the view and leaving the directory, such a
        // listing would map them again, and the duplicate check would go on
        // finding what is gone. No listing runs in between: the refresh lock
        // is held until the files are gone.
        let _no_refresh = unpoison(self.refresh_mu.lock());
        {
            // Concurrent readers hold Arc clones, never the Vec itself, so
            // retaining in place under the write lock is safe (Go rebuilds a
            // fresh slice because its readers may hold the backing array).
            let mut sh = unpoison(self.shared.write());
            sh.sealed.retain(|g| !victim_ids.contains(&g.id));
        }
        if let Some(hook) = unpoison(self.hooks.after_detach.lock()).as_ref() {
            hook();
        }
        let mut first_err: Option<Error> = None;
        for g in victims {
            stats.bytes_freed += g.mm.len() as u64; // whole file, footer included
            if let Err(e) = fs::remove_file(&g.path) {
                first_err.get_or_insert(e.into());
            }
        }
        stats.segments_compacted = victims.len();
        if self.cfg.sync
            && let Some(dir_f) = ap.dir_f.as_ref()
            && let Err(e) = dir_f.sync_all()
        {
            first_err.get_or_insert(e.into());
        }
        first_err.map_or(Ok(()), Err)
    }
}

/// Checks `rec` against its index entry — framing CRC, key match, and the
/// payload rehashed against the key — wrapping any failure with the segment
/// path and offset (Go: `verifyRecord`).
fn verify_record(g: &SealedSegment, e: IndexEntry, rec: &[u8]) -> Result<(), Error> {
    check_record(e.k, rec).map_err(|(msg, verify)| Error::Corrupt {
        msg: format!(
            "amberpack: corrupt pack data: {}: record at offset {}: {msg}",
            g.path.display(),
            e.off
        ),
        verify,
    })
}

/// The full record scrub: framing CRC, key match, and the payload rehashed
/// against the key. Returns the diagnostic message plus whether the failure
/// is an object-verification one (Go: `checkRecord`, whose `verifyObject`
/// failures wrap `ErrVerify`; the outer wrap adds `ErrCorrupt`).
fn check_record(k: Key, rec: &[u8]) -> Result<(), (String, bool)> {
    let parsed = parse_record(rec).map_err(|e| (e.to_string(), false))?;
    if parsed.key != k {
        return Err((
            format!("record keyed {}, expected {}", parsed.key, k),
            false,
        ));
    }
    let payload = decode_payload(parsed.flags, parsed.ulen, &rec[REC_HEADER_SIZE..])
        .map_err(|e| (e.to_string(), false))?;
    verify_object(k, &payload).map_err(|msg| (msg, true))
}
