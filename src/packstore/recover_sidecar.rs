//! Recovery of an active segment from its sidecar index, and the incremental
//! form readers use to follow another writer (Go:
//! `packstore/recover_sidecar.go`). Positions are signed 64-bit integers, as
//! in Go, so that every comparison on a crafted or damaged entry comes out as
//! it does there.

use std::collections::HashMap;
use std::fs::{self, File};
use std::io;
use std::os::unix::fs::FileExt;
use std::path::Path;

use crate::amberpack::{REC_HEADER_SIZE, parse_record};
use crate::key::Key;

use super::footer::TRAILER_SIZE;
use super::recover::{ActiveLoc, scan_active};
use super::sidecar::{
    SIDECAR_ENTRY, SIDECAR_MAGIC, SIDECAR_REC_SIZE, SIDECAR_SUFFIX, SIDECAR_SYNCED, SidecarRec,
    SidecarWriter, read_sidecar,
};
use super::view::with_suffix;
use super::{MAGIC_HEADER, MAGIC_TRAILER, TAG_SEAL};

/// Random-access reads of a data file; a byte slice stands in for one in
/// tests (Go: `io.ReaderAt`).
pub(crate) trait DataAt {
    fn read_exact_at(&self, buf: &mut [u8], off: u64) -> io::Result<()>;
}

impl DataAt for File {
    fn read_exact_at(&self, buf: &mut [u8], off: u64) -> io::Result<()> {
        FileExt::read_exact_at(self, buf, off)
    }
}

impl DataAt for [u8] {
    fn read_exact_at(&self, buf: &mut [u8], off: u64) -> io::Result<()> {
        let end = usize::try_from(off)
            .ok()
            .and_then(|o| o.checked_add(buf.len()))
            .filter(|end| *end <= self.len())
            .ok_or_else(|| io::Error::from(io::ErrorKind::UnexpectedEof))?;
        buf.copy_from_slice(&self[end - buf.len()..end]);
        Ok(())
    }
}

/// How far an active segment has been read: the end of the valid data indexed
/// so far, how much of the sidecar was consumed, and the data length known
/// durable (Go: the scalar fields of `segmentScan`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ScanPos {
    pub pos: i64,
    pub sidecar_end: i64,
    pub durable: i64,
}

/// An active segment's index and how far it has been read. An owner builds
/// one at adoption; a reader keeps one per foreign segment and takes it
/// forward as the owner appends (Go: `segmentScan`).
#[derive(Debug)]
pub(crate) struct SegmentScan {
    pub index: HashMap<Key, ActiveLoc>,
    pub at: ScanPos,
}

/// What an active segment holds, worked out from its sidecar and its data
/// file. Working it out never writes; the segment's owner applies it
/// (truncating the data, bringing the sidecar in line), a reader just uses
/// the index (Go: `recovered`).
#[derive(Debug)]
pub(crate) struct Recovered {
    pub index: HashMap<Key, ActiveLoc>,
    /// End of the valid data; short of the header: it never became durable.
    pub data_end: i64,
    /// Leading sidecar bytes that agree with the data; 0: start the sidecar over.
    pub sidecar_end: i64,
    /// Data length the sidecar knows durable.
    pub durable: i64,
    /// Valid records the agreeing part of the sidecar does not list, in data order.
    pub missing: Vec<SidecarRec>,
    /// The data file carries a complete footer: a seal's rename is outstanding.
    pub sealed: bool,
    /// The data file's length when it was looked at.
    pub file_size: i64,
}

impl Recovered {
    /// Go: `scan`.
    pub(crate) fn scan(self) -> SegmentScan {
        SegmentScan {
            index: self.index,
            at: ScanPos {
                pos: self.data_end,
                sidecar_end: self.sidecar_end,
                durable: self.durable,
            },
        }
    }
}

/// Recovers the active segment at `path` by the sidecar's reading rules
/// (`sidecar.rs`), falling back to a full scan of the data when the sidecar
/// cannot be used (Go: `recoverSegment`).
pub(crate) fn recover_segment(path: &Path) -> io::Result<Recovered> {
    // The sidecar first, the data's length after: a live owner writes a
    // record before its entry, so the data read later covers whatever the
    // sidecar read earlier speaks of. An unreadable sidecar is a missing
    // one: the data file is the truth.
    let sidecar = fs::read(with_suffix(path, SIDECAR_SUFFIX)).unwrap_or_default();
    let f = File::open(path)?;
    let size = f.metadata()?.len() as i64;
    if let Some(mut res) = recover_from(&f, size, &sidecar)? {
        res.file_size = size;
        return Ok(res);
    }
    let mut res = full_scan(path)?;
    res.file_size = size;
    Ok(res)
}

/// Recovery without a sidecar: every record is read and checked (Go:
/// `fullScan`).
pub(crate) fn full_scan(path: &Path) -> io::Result<Recovered> {
    let res = scan_active(path)?;
    let mut missing: Vec<SidecarRec> = res
        .index
        .iter()
        .map(|(k, loc)| SidecarRec::entry(*k, *loc))
        .collect();
    missing.sort_by_key(|r| r.off);
    Ok(Recovered {
        index: res.index,
        data_end: res.size as i64,
        sidecar_end: 0,
        durable: MAGIC_HEADER.len() as i64,
        missing,
        sealed: res.sealed,
        file_size: 0,
    })
}

/// Applies the reading rules to a data file of the given size and the bytes
/// of its sidecar. `None` when the sidecar cannot be relied on at all, or the
/// file looks like a crashed seal: the caller then scans it whole (Go:
/// `recoverFrom`).
pub(crate) fn recover_from<D: DataAt + ?Sized>(
    data: &D,
    size: i64,
    sidecar: &[u8],
) -> io::Result<Option<Recovered>> {
    let header_len = MAGIC_HEADER.len() as i64;
    if read_sidecar(sidecar, true).1 == 0 || size < header_len {
        return Ok(None);
    }
    if read_range(data, 0, header_len)? != MAGIC_HEADER {
        return Ok(None);
    }
    if size >= header_len + TRAILER_SIZE as i64 {
        let n = MAGIC_TRAILER.len() as i64;
        if read_range(data, size - n, n)? == MAGIC_TRAILER {
            return Ok(None); // a footer: the full scan decides whether it is whole
        }
    }
    let mut index = HashMap::new();
    let at = ScanPos {
        pos: header_len,
        sidecar_end: 0,
        durable: header_len,
    };
    let Some(adv) = advance(&index, at, data, size, sidecar)? else {
        return Ok(None);
    };
    for r in &adv.added {
        index.insert(r.k, r.loc());
    }
    Ok(Some(Recovered {
        index,
        data_end: adv.next.pos,
        sidecar_end: adv.next.sidecar_end,
        durable: adv.next.durable,
        missing: adv.missing,
        sealed: false,
        file_size: 0,
    }))
}

/// One step of a scan: the index entries to add, those among them the sidecar
/// does not list, and where the scan stands afterwards.
#[derive(Debug)]
pub(crate) struct Advanced {
    pub added: Vec<SidecarRec>,
    pub missing: Vec<SidecarRec>,
    pub next: ScanPos,
}

/// Takes a scan forward over what was appended since it last ran: `tail`
/// holds the sidecar's bytes from `at.sidecar_end` on, `size` is the data
/// file's length, read after the sidecar was. Nothing is modified: the caller
/// applies the entries and the new position together, so that a shared view
/// can be extended under its own lock, all of it or none. `None` when sidecar
/// and data contradict what the scan already holds: the caller starts over.
///
/// Entries whose record ends within the data known durable (the largest
/// synced record) are trusted unread. Later entries are verified against the
/// data, in order, until one fails. What follows the last good entry is
/// scanned record by record, which finds records written — perhaps
/// acknowledged — just before a crash kept their entries from the sidecar
/// (Go: `(*segmentScan).advance`).
pub(crate) fn advance<D: DataAt + ?Sized>(
    index: &HashMap<Key, ActiveLoc>,
    at: ScanPos,
    data: &D,
    size: i64,
    tail: &[u8],
) -> io::Result<Option<Advanced>> {
    let unchanged = || Advanced {
        added: Vec::new(),
        missing: Vec::new(),
        next: at,
    };
    let header_len = MAGIC_HEADER.len() as i64;
    let mut pos = at.pos;
    if pos < header_len {
        // A view of a segment whose header had not arrived yet.
        if size < header_len {
            return Ok(Some(unchanged()));
        }
        if read_range(data, 0, header_len)? != MAGIC_HEADER {
            return Ok(Some(unchanged()));
        }
        pos = header_len;
    }

    let at_start = at.sidecar_end == 0;
    let (recs, valid) = read_sidecar(tail, at_start);
    let mut consumed = 0i64;
    if at_start && valid > 0 {
        consumed = SIDECAR_MAGIC.len() as i64;
    }
    let mut durable = at.durable;
    for r in &recs {
        if r.kind != SIDECAR_SYNCED {
            continue;
        }
        if r.off_i64() > size || r.off_i64() < 0 {
            // More durable data than the file holds: whatever happened to
            // the file, this sidecar does not describe it.
            return Ok(None);
        }
        durable = durable.max(r.off_i64());
    }
    let mut added = Vec::new();
    for r in &recs {
        if r.kind == SIDECAR_ENTRY {
            if r.off_i64() < pos {
                // Indexed already, by an earlier scan of the data's tail:
                // the entry arrived after the record. Anything else is a
                // contradiction.
                if index.get(&r.k) != Some(&r.loc()) {
                    return Ok(None);
                }
            } else if r.off_i64() == pos && r.end() <= size && r.end() > pos {
                if r.end() > durable && !verify_entry(data, r)? {
                    break;
                }
                added.push(*r);
                pos = r.end();
            } else {
                // Records are contiguous and each has its entry, so one that
                // does not start where the last ended, or runs past the
                // file, ends what the sidecar can vouch for.
                break;
            }
        }
        consumed += SIDECAR_REC_SIZE as i64;
    }
    let sidecar_end = at.sidecar_end + consumed;

    let mut missing = Vec::new();
    if pos < size {
        let rest = read_range(data, pos, size - pos)?;
        let mut off = 0usize;
        // A seal marker without a whole footer: a torn seal.
        while off < rest.len() && rest[off] != TAG_SEAL {
            let Ok(rec) = parse_record(&rest[off..]) else {
                break; // invalid, truncated or still being written: nothing past here counts yet
            };
            let r = SidecarRec::entry(
                rec.key,
                ActiveLoc {
                    off: (pos + off as i64) as u64,
                    flags: rec.flags,
                    ulen: rec.ulen,
                    slen: rec.slen,
                },
            );
            added.push(r);
            missing.push(r);
            off += REC_HEADER_SIZE + rec.slen as usize;
        }
        pos += off as i64;
    }
    Ok(Some(Advanced {
        added,
        missing,
        next: ScanPos {
            pos,
            sidecar_end,
            durable,
        },
    }))
}

/// Reads the record an entry points at and checks that it is whole and is
/// the record the entry describes (Go: `verifyEntry`).
fn verify_entry<D: DataAt + ?Sized>(data: &D, r: &SidecarRec) -> io::Result<bool> {
    let raw = read_range(data, r.off_i64(), r.end() - r.off_i64())?;
    Ok(parse_record(&raw).is_ok_and(|rec| {
        rec.key == r.k && rec.flags == r.flags && rec.ulen == r.ulen && rec.slen == r.slen
    }))
}

/// Go: `readRange`.
pub(crate) fn read_range<D: DataAt + ?Sized>(data: &D, off: i64, n: i64) -> io::Result<Vec<u8>> {
    let (Ok(off), Ok(n)) = (u64::try_from(off), usize::try_from(n)) else {
        return Err(io::Error::from(io::ErrorKind::UnexpectedEof));
    };
    let mut b = vec![0u8; n];
    data.read_exact_at(&mut b, off)?;
    Ok(b)
}

/// Brings a recovered segment's sidecar in line for its new owner: it keeps
/// the part that agrees with the data, drops the rest and lists the records
/// that were missing. It never fails the open. A sidecar that cannot be
/// written must not stay as it is — recovery would go on trusting it — so it
/// is removed, and the segment runs without one (Go: `openOwnedSidecar`).
pub(crate) fn open_owned_sidecar(data_path: &Path, res: &Recovered) -> Option<SidecarWriter> {
    let path = with_suffix(data_path, SIDECAR_SUFFIX);
    match SidecarWriter::open_at(&path, res.sidecar_end.max(0) as u64) {
        Ok(mut w) => {
            for r in &res.missing {
                w.write(r);
            }
            Some(w)
        }
        Err(_) => {
            let _ = fs::remove_file(&path);
            None
        }
    }
}
