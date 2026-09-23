//! The sidecar index of an active segment (Go: `packstore/sidecar.go`).
//!
//! An active segment's index lives in its owner's memory. The sidecar,
//! `<id>.seg.active.idx`, mirrors it on disk as an append-only log, so that
//! opening the segment — by the next owner or by any reader — does not have
//! to parse the data file: an 8-byte magic, then fixed-size big-endian
//! records,
//!
//! ```text
//! offset  size  field
//! 0       1     kind   SIDECAR_ENTRY or SIDECAR_SYNCED
//! 1       32    key    entry: the record's key            synced: zero
//! 33      8     off    entry: offset of the record header synced: data length known durable
//! 41      1     flags  entry: the record's flags byte     synced: zero
//! 42      4     ulen   entry: uncompressed payload length synced: zero
//! 46      4     slen   entry: stored payload length       synced: zero
//! 50      2     zero   reserved
//! 52      4     crc    CRC-32C of bytes [0:52]
//! ```
//!
//! The owner appends an entry only after the record's write to the data file
//! returned, and a synced record only after an fsync of the data file
//! returned. The sidecar itself is never fsynced: it is a cache, and recovery
//! (`recover_sidecar.rs`) trusts entries up to the last synced record,
//! verifies the rest against the data, and scans what the sidecar does not
//! cover. See `architecture/packstore.md`.

use std::fs::{File, OpenOptions};
use std::io;
use std::os::unix::fs::FileExt;
use std::path::Path;

use crate::amberpack::REC_HEADER_SIZE;
use crate::key::{self, Key};

use super::recover::ActiveLoc;

/// Appended to the active segment's file name (Go: `sidecarSuffix`).
pub(crate) const SIDECAR_SUFFIX: &str = ".idx";
/// The size of one sidecar record (Go: `sidecarRecSize`).
pub(crate) const SIDECAR_REC_SIZE: usize = 56;
/// Record kind: an index entry (Go: `sidecarEntry`).
pub(crate) const SIDECAR_ENTRY: u8 = 0x01;
/// Record kind: a durability mark (Go: `sidecarSynced`).
pub(crate) const SIDECAR_SYNCED: u8 = 0x02;
/// The 8-byte sidecar file header (Go: `sidecarMagic`).
pub(crate) const SIDECAR_MAGIC: [u8; 8] = *b"AMBERIX\x01";

/// One sidecar record (Go: `sidecarRec`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct SidecarRec {
    pub kind: u8,
    pub k: Key,
    pub off: u64,
    pub flags: u8,
    pub ulen: u32,
    pub slen: u32,
}

impl SidecarRec {
    /// The entry for the record of `k` at `loc` (Go: `entryRec`).
    pub(crate) fn entry(k: Key, loc: ActiveLoc) -> SidecarRec {
        SidecarRec {
            kind: SIDECAR_ENTRY,
            k,
            off: loc.off,
            flags: loc.flags,
            ulen: loc.ulen,
            slen: loc.slen,
        }
    }

    /// The mark that the first `data_len` bytes of the data file are durable.
    pub(crate) fn synced(data_len: u64) -> SidecarRec {
        SidecarRec {
            kind: SIDECAR_SYNCED,
            k: Key([0u8; key::SIZE]),
            off: data_len,
            flags: 0,
            ulen: 0,
            slen: 0,
        }
    }

    /// An entry's place in the data file (Go: `loc`).
    pub(crate) fn loc(&self) -> ActiveLoc {
        ActiveLoc {
            off: self.off,
            flags: self.flags,
            ulen: self.ulen,
            slen: self.slen,
        }
    }

    /// The offset as Go sees it, a signed 64-bit integer: an offset past
    /// `i64::MAX` reads as negative there, and recovery's comparisons follow
    /// Go's.
    pub(crate) fn off_i64(&self) -> i64 {
        self.off as i64
    }

    /// The data offset just past an entry's record, in Go's wrapping signed
    /// arithmetic (Go: `end`).
    pub(crate) fn end(&self) -> i64 {
        self.off_i64()
            .wrapping_add(REC_HEADER_SIZE as i64)
            .wrapping_add(i64::from(self.slen))
    }

    /// Go: `encode`.
    pub(crate) fn encode(&self) -> [u8; SIDECAR_REC_SIZE] {
        let mut b = [0u8; SIDECAR_REC_SIZE];
        b[0] = self.kind;
        b[1..33].copy_from_slice(&self.k.0);
        b[33..41].copy_from_slice(&self.off.to_be_bytes());
        b[41] = self.flags;
        b[42..46].copy_from_slice(&self.ulen.to_be_bytes());
        b[46..50].copy_from_slice(&self.slen.to_be_bytes());
        let crc = crc32c::crc32c(&b[..52]);
        b[52..56].copy_from_slice(&crc.to_be_bytes());
        b
    }

    /// Parses one record, reporting `None` for anything the encoder would
    /// not have produced (Go: `decodeSidecarRec`).
    pub(crate) fn decode(b: &[u8]) -> Option<SidecarRec> {
        if b.len() < SIDECAR_REC_SIZE {
            return None;
        }
        let crc = u32::from_be_bytes([b[52], b[53], b[54], b[55]]);
        if crc32c::crc32c(&b[..52]) != crc {
            return None;
        }
        let mut kb = [0u8; key::SIZE];
        kb.copy_from_slice(&b[1..33]);
        let r = SidecarRec {
            kind: b[0],
            k: Key(kb),
            off: u64::from_be_bytes([b[33], b[34], b[35], b[36], b[37], b[38], b[39], b[40]]),
            flags: b[41],
            ulen: u32::from_be_bytes([b[42], b[43], b[44], b[45]]),
            slen: u32::from_be_bytes([b[46], b[47], b[48], b[49]]),
        };
        if b[50] != 0 || b[51] != 0 {
            return None;
        }
        match r.kind {
            SIDECAR_ENTRY => {}
            SIDECAR_SYNCED => {
                if r.k.0 != [0u8; key::SIZE] || r.flags != 0 || r.ulen != 0 || r.slen != 0 {
                    return None;
                }
            }
            _ => return None,
        }
        Some(r)
    }
}

/// Parses `b`: a whole sidecar file (`at_start`), or the bytes from a record
/// boundary on. It returns the records up to the first invalid or partial one
/// and the number of bytes of `b` they cover, magic included. A bad magic
/// yields nothing (Go: `readSidecar`).
pub(crate) fn read_sidecar(b: &[u8], at_start: bool) -> (Vec<SidecarRec>, usize) {
    let mut off = 0;
    if at_start {
        if b.len() < SIDECAR_MAGIC.len() || b[..SIDECAR_MAGIC.len()] != SIDECAR_MAGIC {
            return (Vec::new(), 0);
        }
        off = SIDECAR_MAGIC.len();
    }
    let mut recs = Vec::new();
    while off + SIDECAR_REC_SIZE <= b.len() {
        let Some(r) = SidecarRec::decode(&b[off..off + SIDECAR_REC_SIZE]) else {
            break;
        };
        recs.push(r);
        off += SIDECAR_REC_SIZE;
    }
    (recs, off)
}

/// Appends to a sidecar. A failed write never fails the store's write — the
/// data is intact, and recovery copes with a short index — but it stops the
/// writer for good, so that no record ever follows a hole. A segment without
/// a sidecar holds `None` where Go holds a nil writer (Go: `sidecarWriter`).
#[derive(Debug)]
pub(crate) struct SidecarWriter {
    f: File,
    off: u64,
    broken: bool,
}

impl SidecarWriter {
    /// Starts `path` over: magic only (Go: `createSidecar`).
    pub(crate) fn create(path: &Path) -> io::Result<SidecarWriter> {
        let f = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(true)
            .open(path)?;
        f.write_all_at(&SIDECAR_MAGIC, 0)?;
        Ok(SidecarWriter {
            f,
            off: SIDECAR_MAGIC.len() as u64,
            broken: false,
        })
    }

    /// Keeps the first `valid` bytes of `path` — what recovery found to agree
    /// with the data — drops the rest and appends after them. A valid length
    /// short of the magic starts the file over (Go: `openSidecarAt`).
    pub(crate) fn open_at(path: &Path, valid: u64) -> io::Result<SidecarWriter> {
        if valid < SIDECAR_MAGIC.len() as u64 {
            return SidecarWriter::create(path);
        }
        let f = OpenOptions::new().read(true).write(true).open(path)?;
        f.set_len(valid)?;
        Ok(SidecarWriter {
            f,
            off: valid,
            broken: false,
        })
    }

    /// Go: `write`.
    pub(crate) fn write(&mut self, r: &SidecarRec) {
        if self.broken {
            return;
        }
        if self.f.write_all_at(&r.encode(), self.off).is_err() {
            self.broken = true;
            return;
        }
        self.off += SIDECAR_REC_SIZE as u64;
    }

    /// Records that the record for `k` was written at `loc` (Go: `entry`).
    pub(crate) fn entry(&mut self, k: Key, loc: ActiveLoc) {
        self.write(&SidecarRec::entry(k, loc));
    }

    /// Records that an fsync made the first `data_len` bytes of the data file
    /// durable (Go: `synced`).
    pub(crate) fn synced(&mut self, data_len: u64) {
        self.write(&SidecarRec::synced(data_len));
    }

    /// A writer over a file of the test's choosing, say one that refuses writes.
    #[cfg(test)]
    pub(crate) fn over(f: File, off: u64) -> SidecarWriter {
        SidecarWriter {
            f,
            off,
            broken: false,
        }
    }

    #[cfg(test)]
    pub(crate) fn state(&self) -> (u64, bool) {
        (self.off, self.broken)
    }
}
