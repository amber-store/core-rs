//! The sealed-segment footer: fanout index section, binary fuse filter
//! section, fixed trailer, and the mmap'd sealed segment built on them (Go:
//! `packstore/footer.go`).

use std::fs::File;
use std::path::{Path, PathBuf};

use memmap2::Mmap;

use crate::amberpack::{REC_HEADER_SIZE, decode_payload};
use crate::binaryfuse::{BinaryFuse16, SECTION_HEADER_SIZE};
use crate::key::{self, Key};

use super::view::FileIdent;
use super::{Error, MAGIC_HEADER, MAGIC_TRAILER, TAG_SEAL, be_u32, corrupt};

/// 256 cumulative u32 counts on the key's last byte.
pub(crate) const FANOUT_SIZE: usize = 256 * 4;
/// One index row: key (32) + offset (8) + stored length (4).
pub(crate) const INDEX_ENTRY_SIZE: usize = 32 + 8 + 4;
/// The fixed trailer at the very end of a sealed segment.
pub(crate) const TRAILER_SIZE: usize = 64;

/// The smallest possible sealed-segment file: header, seal tag, one-entry
/// index, filter header, trailer.
const MIN_SEALED_LEN: usize =
    MAGIC_HEADER.len() + 1 + FANOUT_SIZE + INDEX_ENTRY_SIZE + SECTION_HEADER_SIZE + TRAILER_SIZE;

/// One sealed-segment index row: a key and where its record starts, plus the
/// stored payload length (authoritative for reads; footer-CRC-protected and
/// cross-checked against the body by scrub) (Go: `indexEntry`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct IndexEntry {
    pub k: Key,
    pub off: u64, // file offset of the record header
    pub slen: u32,
}

/// Orders by (last key byte, full key): the fanout is on the last byte
/// because byte 0 is type/length-size and clusters, while the hash tail is
/// uniformly distributed (Go: `compareEntries`).
fn compare_entries(a: &IndexEntry, b: &IndexEntry) -> std::cmp::Ordering {
    a.k.as_bytes()[key::SIZE - 1]
        .cmp(&b.k.as_bytes()[key::SIZE - 1])
        .then_with(|| a.k.as_bytes().cmp(b.k.as_bytes()))
}

/// Serializes the index section (fanout + sorted entries). It does not mutate
/// `entries`. Callers must pass entries with distinct keys (the write path
/// dedups; with duplicate keys the relative order of their rows is
/// unspecified) (Go: `buildIndexSection`).
pub(crate) fn build_index_section(entries: &[IndexEntry]) -> Vec<u8> {
    let mut es = entries.to_vec();
    es.sort_unstable_by(compare_entries);

    let mut out = vec![0u8; FANOUT_SIZE + es.len() * INDEX_ENTRY_SIZE];
    let mut counts = [0u32; 256];
    for e in &es {
        counts[e.k.as_bytes()[key::SIZE - 1] as usize] += 1;
    }
    let mut cum = 0u32;
    for (b, count) in counts.iter().enumerate() {
        cum += count;
        out[b * 4..b * 4 + 4].copy_from_slice(&cum.to_be_bytes());
    }
    let mut off = FANOUT_SIZE;
    for e in &es {
        out[off..off + 32].copy_from_slice(e.k.as_bytes());
        out[off + 32..off + 40].copy_from_slice(&e.off.to_be_bytes());
        out[off + 40..off + 44].copy_from_slice(&e.slen.to_be_bytes());
        off += INDEX_ENTRY_SIZE;
    }
    out
}

/// Splits an index section into a decoded fanout table and the raw entry
/// bytes, validating lengths and fanout monotonicity (Go:
/// `parseIndexSection`).
pub(crate) fn parse_index_section(b: &[u8], key_count: u64) -> Result<([u32; 256], &[u8]), Error> {
    if key_count > u64::from(u32::MAX) {
        return Err(corrupt(format!(
            "key count {key_count} exceeds format limit"
        )));
    }
    let want = FANOUT_SIZE as u64 + key_count * INDEX_ENTRY_SIZE as u64;
    if b.len() as u64 != want {
        return Err(corrupt(format!(
            "index section is {} bytes, want {want}",
            b.len()
        )));
    }
    let mut fanout = [0u32; 256];
    let mut prev = 0u32;
    for (i, f) in fanout.iter_mut().enumerate() {
        *f = be_u32(b, i * 4);
        if *f < prev {
            return Err(corrupt(format!("fanout not monotonic at byte {i:#x}")));
        }
        prev = *f;
    }
    if u64::from(fanout[255]) != key_count {
        return Err(corrupt(format!(
            "fanout total {} != key count {key_count}",
            fanout[255]
        )));
    }
    Ok((fanout, &b[FANOUT_SIZE..]))
}

/// The filter input for `k`: the last 8 bytes of the key, which lie in the
/// uniformly distributed truncated-hash region (Go: `filterKey`).
pub(crate) fn filter_key(k: Key) -> u64 {
    let b = k.as_bytes();
    u64::from_be_bytes([
        b[key::SIZE - 8],
        b[key::SIZE - 7],
        b[key::SIZE - 6],
        b[key::SIZE - 5],
        b[key::SIZE - 4],
        b[key::SIZE - 3],
        b[key::SIZE - 2],
        b[key::SIZE - 1],
    ])
}

/// Builds and serializes a binary fuse filter over the entries' keys.
/// Duplicate 8-byte tails are deduplicated before the build (Go:
/// `buildFilterSection`).
pub(crate) fn build_filter_section(entries: &[IndexEntry]) -> Result<Vec<u8>, Error> {
    let mut tails: Vec<u64> = entries.iter().map(|e| filter_key(e.k)).collect();
    tails.sort_unstable();
    tails.dedup();
    let f = BinaryFuse16::new(&tails)
        .map_err(|e| Error::Other(format!("packstore: building fuse filter: {e}")))?;
    Ok(f.section_bytes())
}

/// Deserializes a filter section, copying fingerprints out of `b` (which may
/// be a read-only mmap) into RAM. The geometry fields are validated so
/// crafted values fail parse with a corrupt error rather than panicking the
/// read path (Go: `parseFilterSection`).
pub(crate) fn parse_filter_section(b: &[u8]) -> Result<BinaryFuse16, Error> {
    BinaryFuse16::parse_section(b).map_err(corrupt)
}

/// Finds `k` in a parsed index section: fanout bucket on the last byte, then
/// binary search on the full key within the bucket (Go: `searchIndex`).
pub(crate) fn search_index(fanout: &[u32; 256], entries: &[u8], k: Key) -> Option<(u64, u32)> {
    let pos = search_index_pos(fanout, entries, k)?;
    let e = &entries[pos * INDEX_ENTRY_SIZE..(pos + 1) * INDEX_ENTRY_SIZE];
    let off = u64::from_be_bytes([e[32], e[33], e[34], e[35], e[36], e[37], e[38], e[39]]);
    Some((off, be_u32(e, 40)))
}

/// Returns `k`'s entry position within the index section (Go:
/// `searchIndexPos`).
pub(crate) fn search_index_pos(fanout: &[u32; 256], entries: &[u8], k: Key) -> Option<usize> {
    let b = k.as_bytes()[key::SIZE - 1];
    let lo = if b > 0 { fanout[b as usize - 1] } else { 0 } as usize;
    let n = fanout[b as usize] as usize - lo;
    let row = |i: usize| &entries[(lo + i) * INDEX_ENTRY_SIZE..(lo + i + 1) * INDEX_ENTRY_SIZE];
    // partition_point == sort.Search: first i with row key >= k.
    let (mut low, mut high) = (0usize, n);
    while low < high {
        let mid = (low + high) / 2;
        if row(mid)[..32] < k.as_bytes()[..] {
            low = mid + 1;
        } else {
            high = mid;
        }
    }
    if low >= n {
        return None;
    }
    let pos = lo + low;
    if entries[pos * INDEX_ENTRY_SIZE..pos * INDEX_ENTRY_SIZE + 32] != k.as_bytes()[..] {
        return None;
    }
    Some(pos)
}

/// Assembles the complete footer (seal marker, index section, filter section,
/// trailer) for a segment whose records end at `body_len` (Go: `buildFooter`).
pub(crate) fn build_footer(body_len: u64, entries: &[IndexEntry]) -> Result<Vec<u8>, Error> {
    if entries.is_empty() {
        return Err(Error::Other(
            "packstore: refusing to seal an empty segment".into(),
        ));
    }
    let idx = build_index_section(entries);
    let filt = build_filter_section(entries)?;
    let mut ftr = Vec::with_capacity(1 + idx.len() + filt.len() + TRAILER_SIZE);
    ftr.push(TAG_SEAL);
    ftr.extend_from_slice(&idx);
    ftr.extend_from_slice(&filt);

    let mut tr = [0u8; TRAILER_SIZE];
    let index_off = body_len + 1;
    tr[0..8].copy_from_slice(&index_off.to_be_bytes());
    tr[8..16].copy_from_slice(&(idx.len() as u64).to_be_bytes());
    tr[16..24].copy_from_slice(&(index_off + idx.len() as u64).to_be_bytes());
    tr[24..32].copy_from_slice(&(filt.len() as u64).to_be_bytes());
    tr[32..40].copy_from_slice(&(entries.len() as u64).to_be_bytes());
    tr[40..48].copy_from_slice(&body_len.to_be_bytes());
    tr[56..64].copy_from_slice(&MAGIC_TRAILER);
    ftr.extend_from_slice(&tr);
    // The footer CRC covers [body_len, EOF-16): everything up to and
    // excluding the crc field itself; reserved and magic are checked
    // explicitly on parse.
    let crc_at = ftr.len() - 16;
    let crc = crc32c::crc32c(&ftr[..crc_at]);
    ftr[crc_at..crc_at + 4].copy_from_slice(&crc.to_be_bytes());
    Ok(ftr)
}

/// The parsed footer of a sealed segment. `fanout` and `filter` live in RAM;
/// the entry rows stay in the segment image at
/// `entries_off..entries_off + entries_len` (Go: `footerView`, whose
/// `entries` points into the mmap).
pub(crate) struct FooterView {
    pub fanout: [u32; 256],
    pub entries_off: usize,
    pub entries_len: usize,
    pub filter: BinaryFuse16,
    pub key_count: u64,
    pub body_len: u64,
    pub index_off: u64,
    pub index_len: u64,
}

impl FooterView {
    /// Finds `k` in the segment's index (Go: `footerView.lookup`).
    pub(crate) fn lookup(&self, image: &[u8], k: Key) -> Option<(u64, u32)> {
        let entries = &image[self.entries_off..self.entries_off + self.entries_len];
        search_index(&self.fanout, entries, k)
    }

    /// Finds `k`'s position in the segment's index — the mark-set bit slot
    /// for the record (Go: `footerView.lookupPos`; see markset.rs).
    pub(crate) fn lookup_pos(&self, image: &[u8], k: Key) -> Option<usize> {
        let entries = &image[self.entries_off..self.entries_off + self.entries_len];
        search_index_pos(&self.fanout, entries, k)
    }
}

/// Validates a whole sealed-segment image (header through trailer) and
/// returns its footer view. `mm` may be a read-only mmap; nothing is mutated
/// (Go: `parseFooter`).
pub(crate) fn parse_footer(mm: &[u8]) -> Result<FooterView, Error> {
    if mm.len() < MIN_SEALED_LEN {
        return Err(corrupt(format!("file too short: {} bytes", mm.len())));
    }
    if mm[..MAGIC_HEADER.len()] != MAGIC_HEADER {
        return Err(corrupt("bad header magic"));
    }
    let tr = &mm[mm.len() - TRAILER_SIZE..];
    if tr[56..64] != MAGIC_TRAILER {
        return Err(corrupt("bad trailer magic"));
    }
    if be_u32(tr, 52) != 0 {
        return Err(corrupt("nonzero reserved trailer field"));
    }
    let read_u64 = |off: usize| {
        u64::from_be_bytes([
            tr[off],
            tr[off + 1],
            tr[off + 2],
            tr[off + 3],
            tr[off + 4],
            tr[off + 5],
            tr[off + 6],
            tr[off + 7],
        ])
    };
    let index_off = read_u64(0);
    let index_len = read_u64(8);
    let filter_off = read_u64(16);
    let filter_len = read_u64(24);
    let key_count = read_u64(32);
    let body_len = read_u64(40);

    let file_len = mm.len() as u64;
    // Checked in Go's order with Go's short-circuiting: each later expression
    // relies on the earlier ones for overflow freedom.
    if body_len < MAGIC_HEADER.len() as u64
        || body_len >= file_len
        || key_count > u64::from(u32::MAX) // fanout counts are u32; also keeps the next line overflow-free
        || index_off != body_len + 1
        || index_len != FANOUT_SIZE as u64 + key_count * INDEX_ENTRY_SIZE as u64
        || filter_off != index_off + index_len
        || filter_off > file_len - TRAILER_SIZE as u64
        || filter_len != file_len - TRAILER_SIZE as u64 - filter_off
    {
        return Err(corrupt("trailer offsets inconsistent"));
    }
    if crc32c::crc32c(&mm[body_len as usize..mm.len() - 16]) != be_u32(tr, 48) {
        return Err(corrupt("footer CRC mismatch"));
    }
    if mm[body_len as usize] != TAG_SEAL {
        return Err(corrupt("missing seal marker"));
    }

    let (fanout, _) = parse_index_section(
        &mm[index_off as usize..(index_off + index_len) as usize],
        key_count,
    )?;
    let filter =
        parse_filter_section(&mm[filter_off as usize..(filter_off + filter_len) as usize])?;
    Ok(FooterView {
        fanout,
        entries_off: index_off as usize + FANOUT_SIZE,
        entries_len: index_len as usize - FANOUT_SIZE,
        filter,
        key_count,
        body_len,
        index_off,
        index_len,
    })
}

/// An immutable, fully mmap'd sealed segment (Go: `sealedSegment`).
pub(crate) struct SealedSegment {
    pub id: u64,
    pub path: PathBuf,
    /// Which file this is: a repair replaces a segment under its name, and
    /// an id can come back (Go: `fi`).
    pub ident: FileIdent,
    pub mm: Mmap,
    pub fv: FooterView,
}

impl std::fmt::Debug for SealedSegment {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SealedSegment")
            .field("id", &self.id)
            .field("path", &self.path)
            .finish_non_exhaustive()
    }
}

impl SealedSegment {
    /// Maps a sealed segment and validates its footer. The fd is closed after
    /// mapping; the mapping keeps the file content alive (Go: `openSealed`).
    pub(crate) fn open(path: &Path, id: u64) -> Result<SealedSegment, Error> {
        let f = File::open(path)?;
        let st = f.metadata()?;
        if st.len() < MIN_SEALED_LEN as u64 {
            return Err(corrupt(format!(
                "{}: file too short: {} bytes",
                path.display(),
                st.len()
            )));
        }
        // SAFETY: the mapping is read-only and the store owns its segment
        // files: sealed segments are immutable by contract (like Go, external
        // truncation of a mapped segment is undefined behavior).
        let mm = unsafe { Mmap::map(&f) }
            .map_err(|e| Error::Other(format!("packstore: mmap {}: {e}", path.display())))?;
        let fv = parse_footer(&mm).map_err(|e| Error::Context {
            msg: path.display().to_string(),
            source: Box::new(e),
        })?;
        Ok(SealedSegment {
            id,
            path: path.to_path_buf(),
            ident: FileIdent::of(&st),
            mm,
            fv,
        })
    }

    /// Reports whether `k` is in this segment: fuse filter first (cheap,
    /// probabilistic), then the exact index (Go: `has`).
    pub(crate) fn has(&self, k: Key) -> bool {
        self.fv.filter.contains(filter_key(k)) && self.fv.lookup(&self.mm, k).is_some()
    }

    /// Bounds-checks an index entry and returns the record's byte range in
    /// the mmap (Go: the shared bounds check in `get` / `getRecord`).
    fn record_span(&self, off: u64, slen: u32) -> Result<(usize, usize), Error> {
        let body_len = self.fv.body_len;
        if off < MAGIC_HEADER.len() as u64
            || off > body_len
            || REC_HEADER_SIZE as u64 + u64::from(slen) > body_len - off
        {
            return Err(corrupt(format!(
                "{}: index entry out of bounds",
                self.path.display()
            )));
        }
        let end = off + REC_HEADER_SIZE as u64 + u64::from(slen);
        Ok((off as usize, end as usize))
    }

    /// Returns `k`'s payload (caller-owned) if present, or a corruption
    /// error. The hot path does not CRC-check; that is scrub's job (Go:
    /// `get`).
    pub(crate) fn get(&self, k: Key) -> Result<Option<Vec<u8>>, Error> {
        if !self.fv.filter.contains(filter_key(k)) {
            return Ok(None);
        }
        let Some((off, slen)) = self.fv.lookup(&self.mm, k) else {
            return Ok(None);
        };
        let (start, end) = self.record_span(off, slen)?;
        let h = &self.mm[start..start + REC_HEADER_SIZE];
        let flags = h[33];
        let ulen = be_u32(h, 34);
        match decode_payload(flags, ulen, &self.mm[start + REC_HEADER_SIZE..end]) {
            Ok(data) => Ok(Some(data)),
            Err(e) => Err(Error::Corrupt {
                msg: format!("{}: {e}", self.path.display()),
                verify: false,
            }),
        }
    }

    /// Returns a caller-owned copy of `k`'s full on-disk record (header +
    /// stored payload, undecoded) if present. Like `get`, the hot path does
    /// not CRC-check; the record is validated by the receiving reader on the
    /// push path (Go: `getRecord`).
    pub(crate) fn get_record(&self, k: Key) -> Result<Option<Vec<u8>>, Error> {
        if !self.fv.filter.contains(filter_key(k)) {
            return Ok(None);
        }
        let Some((off, slen)) = self.fv.lookup(&self.mm, k) else {
            return Ok(None);
        };
        let (start, end) = self.record_span(off, slen)?;
        Ok(Some(self.mm[start..end].to_vec()))
    }

    /// Returns `k`'s stored (post-compression) payload length if present,
    /// from the index alone — no payload read (Go: `storedSize`).
    pub(crate) fn stored_size(&self, k: Key) -> Option<u32> {
        if !self.fv.filter.contains(filter_key(k)) {
            return None;
        }
        self.fv.lookup(&self.mm, k).map(|(_, slen)| slen)
    }

    /// Returns `k`'s record offset within this segment if present, from the
    /// index alone — for ordering reads by disk layout (Go: `locate`).
    pub(crate) fn locate(&self, k: Key) -> Option<u64> {
        if !self.fv.filter.contains(filter_key(k)) {
            return None;
        }
        self.fv.lookup(&self.mm, k).map(|(off, _)| off)
    }
}
