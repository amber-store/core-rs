//! Active-segment tail-scan recovery (Go: `packstore/recover.go`).

use std::collections::HashMap;
use std::fs;
use std::io;
use std::path::Path;

use crate::amberpack::{REC_HEADER_SIZE, parse_record};
use crate::key::Key;

use super::{MAGIC_HEADER, TAG_SEAL, footer::parse_footer};

/// Locates one record inside the active segment (Go: `activeLoc`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ActiveLoc {
    pub off: u64, // record header offset
    pub flags: u8,
    pub ulen: u32,
    pub slen: u32,
}

/// The outcome of tail-scanning an active segment file (Go: `scanResult`).
pub(crate) struct ScanResult {
    /// Valid length; the caller truncates to this (0 ⇒ reset header).
    pub size: u64,
    /// Records fully contained in `[0, size)`.
    pub index: HashMap<Key, ActiveLoc>,
    /// The file carries a complete valid footer: rename it, it is sealed.
    pub sealed: bool,
}

/// Reads an active segment file and finds the boundary of valid data. Records
/// are self-framing and CRC'd, so the scan accepts records until the first
/// invalid byte and truncates there. Acknowledged (fsynced) data is always
/// before that boundary: fsync covers the whole file, so a valid record can
/// only be preceded by valid bytes (Go: `scanActive`).
pub(crate) fn scan_active(path: &Path) -> io::Result<ScanResult> {
    let mut res = ScanResult {
        size: 0,
        index: HashMap::new(),
        sealed: false,
    };
    let b = fs::read(path)?;
    if b.len() < MAGIC_HEADER.len() || b[..MAGIC_HEADER.len()] != MAGIC_HEADER {
        // Header never made it to disk; nothing in this file was ever
        // acknowledged (any successful fsync would have persisted the header
        // too). Reset to empty.
        return Ok(res);
    }
    // A synced footer can precede a crashed seal rename. Its index
    // preserves later records even when an earlier payload has bit rot.
    if parse_footer(&b).is_ok() {
        res.size = b.len() as u64;
        res.sealed = true;
        return Ok(res);
    }
    let mut off = MAGIC_HEADER.len();
    while off < b.len() {
        if b[off] == TAG_SEAL {
            if parse_footer(&b).is_ok() {
                res.size = b.len() as u64;
                res.sealed = true;
                return Ok(res);
            }
            // Partial footer: truncate at the seal marker. A valid footer
            // followed by trailing bytes also lands here (parse_footer
            // anchors the trailer at EOF); that state cannot arise from our
            // write ordering, and truncating it only un-seals, never loses
            // records.
            break;
        }
        let Ok(rec) = parse_record(&b[off..]) else {
            break; // invalid or truncated: everything from off on is garbage
        };
        res.index.insert(
            rec.key,
            ActiveLoc {
                off: off as u64,
                flags: rec.flags,
                ulen: rec.ulen,
                slen: rec.slen,
            },
        );
        off += REC_HEADER_SIZE + rec.slen as usize;
    }
    res.size = off as u64;
    Ok(res)
}
