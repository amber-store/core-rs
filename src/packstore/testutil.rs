//! Shared helpers for packstore unit tests (Go: `helpers_test.go`,
//! `footer_test.go` helpers). The pseudo-random streams use splitmix64 rather
//! than Go's PCG — the tests rely on the *properties* (incompressible /
//! compressible), not the exact bytes.

use std::fs;
use std::path::PathBuf;

use tempfile::TempDir;

use crate::amberpack::{REC_HEADER_SIZE, encode_record};
use crate::key::{Key, Type};

use super::footer::{IndexEntry, TRAILER_SIZE, build_footer};
use super::{MAGIC_HEADER, Object, be_u32};

/// Deterministic splitmix64 stream.
pub(crate) struct Rng(pub u64);

impl Rng {
    pub(crate) fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    pub(crate) fn below(&mut self, n: u64) -> u64 {
        self.next_u64() % n
    }
}

/// A canonical Blob object for `data` (Go: `blobObj`).
pub(crate) fn blob_obj(data: &[u8]) -> Object {
    Object {
        key: Key::new(Type::Blob, data.len() as u64, data),
        data: data.to_vec(),
        record: None,
    }
}

/// n deterministic pseudo-random bytes (zstd cannot shrink them) (Go:
/// `incompressible`).
pub(crate) fn incompressible(n: usize) -> Vec<u8> {
    let mut rng = Rng(0x42_0007);
    let mut out = Vec::with_capacity(n + 8);
    while out.len() < n {
        out.extend_from_slice(&rng.next_u64().to_le_bytes());
    }
    out.truncate(n);
    out
}

/// n highly repetitive bytes (zstd shrinks them a lot) (Go: `compressible`).
pub(crate) fn compressible(n: usize) -> Vec<u8> {
    b"abcdefgh".iter().copied().cycle().take(n).collect()
}

/// n mixed compressible/incompressible ~2 KB objects (Go: `testObjects`).
pub(crate) fn test_objects(n: usize) -> Vec<Object> {
    (0..n)
        .map(|i| {
            let mut data = if i % 2 == 0 {
                compressible(2000)
            } else {
                incompressible(2000)
            };
            data.push(i as u8);
            data.push((i >> 8) as u8);
            blob_obj(&data)
        })
        .collect()
}

/// n index entries with distinct keys and synthetic offsets (Go:
/// `testEntries`).
pub(crate) fn test_entries(n: usize) -> Vec<IndexEntry> {
    (0..n)
        .map(|i| {
            let mut data = incompressible(64);
            data.push(i as u8);
            data.push((i >> 8) as u8);
            data.push((i >> 16) as u8);
            IndexEntry {
                k: Key::new(Type::Blob, data.len() as u64, &data),
                off: 8 + i as u64 * 100,
                slen: i as u32 + 1,
            }
        })
        .collect()
}

/// Assembles a complete sealed segment on disk from objects: header, records,
/// footer. Returns the owning tempdir, the path, and the entries written (Go:
/// `writeSealedFile`).
pub(crate) fn write_sealed_file(objs: &[Object]) -> (TempDir, PathBuf, Vec<IndexEntry>) {
    let (body, entries) = build_body(objs);
    let footer = build_footer(body.len() as u64, &entries).expect("build_footer");
    let dir = TempDir::new().expect("tempdir");
    let path = dir.path().join("0000000000000001.seg");
    let mut file = body;
    file.extend_from_slice(&footer);
    fs::write(&path, &file).expect("write sealed file");
    (dir, path, entries)
}

/// Returns header+records bytes plus each record's index entry (Go:
/// `buildBody`, entries carrying spans).
pub(crate) fn build_body(objs: &[Object]) -> (Vec<u8>, Vec<IndexEntry>) {
    let mut body = MAGIC_HEADER.to_vec();
    let mut entries = Vec::new();
    for o in objs {
        let rec = encode_record(o.key, &o.data).expect("encode_record");
        entries.push(IndexEntry {
            k: o.key,
            off: body.len() as u64,
            slen: (rec.len() - REC_HEADER_SIZE) as u32,
        });
        body.extend_from_slice(&rec);
    }
    (body, entries)
}

/// Recomputes a doctored sealed image's footer CRC so `parse_footer`'s CRC
/// check passes and deeper validation is exercised (Go: `refreshFooterCRC`).
pub(crate) fn refresh_footer_crc(b: &mut [u8]) {
    let tr_at = b.len() - TRAILER_SIZE;
    let body_len = u64::from_be_bytes([
        b[tr_at + 40],
        b[tr_at + 41],
        b[tr_at + 42],
        b[tr_at + 43],
        b[tr_at + 44],
        b[tr_at + 45],
        b[tr_at + 46],
        b[tr_at + 47],
    ]) as usize;
    let crc = crc32c::crc32c(&b[body_len..b.len() - 16]);
    let crc_at = b.len() - 16;
    b[crc_at..crc_at + 4].copy_from_slice(&crc.to_be_bytes());
}

/// Reads the big-endian u64 at `off` in `b`.
pub(crate) fn be_u64(b: &[u8], off: usize) -> u64 {
    (u64::from(be_u32(b, off)) << 32) | u64::from(be_u32(b, off + 4))
}
