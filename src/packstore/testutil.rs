//! Shared helpers for packstore unit tests (Go: `helpers_test.go`,
//! `footer_test.go` helpers). The pseudo-random streams use splitmix64 rather
//! than Go's PCG — the tests rely on the *properties* (incompressible /
//! compressible), not the exact bytes.

use std::fs::{self, OpenOptions};
use std::os::unix::fs::FileExt;
use std::path::{Path, PathBuf};

use tempfile::TempDir;

use crate::amberpack::{REC_HEADER_SIZE, encode_record};
use crate::key::{Key, Type};

use super::footer::{IndexEntry, TRAILER_SIZE, build_footer};
use super::recover::scan_active;
use super::sidecar::{SIDECAR_MAGIC, SIDECAR_SUFFIX, SidecarRec, read_sidecar};
use super::view::with_suffix;
use super::{MAGIC_HEADER, Object, Store, be_u32};

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

/// Go: `putAll`.
pub(crate) fn put_all(s: &Store, objs: &[Object]) {
    for o in objs {
        s.put(o.key, &o.data).unwrap();
    }
}

/// Go: `wantObjects`.
pub(crate) fn want_objects(s: &Store, objs: &[Object]) {
    for o in objs {
        match s.get(o.key) {
            Ok(got) => assert!(
                got == o.data,
                "get({}): {} bytes, want the {} bytes put",
                o.key,
                got.len(),
                o.data.len()
            ),
            Err(e) => panic!("get({}): {e}", o.key),
        }
    }
}

/// The files in `dir` whose names end in `suffix`, sorted.
pub(crate) fn files_with_suffix(dir: &Path, suffix: &str) -> Vec<PathBuf> {
    let mut out: Vec<_> = fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().path())
        .filter(|p| p.file_name().unwrap().to_string_lossy().ends_with(suffix))
        .collect();
    out.sort();
    out
}

/// The path of the directory's single active segment (Go: `onlyActive`).
pub(crate) fn only_active(dir: &Path) -> PathBuf {
    let actives = files_with_suffix(dir, ".seg.active");
    assert_eq!(
        actives.len(),
        1,
        "active segments = {actives:?}; want exactly one"
    );
    actives[0].clone()
}

/// The sidecar of the active segment at `data`.
pub(crate) fn sidecar_of(data: &Path) -> PathBuf {
    with_suffix(data, SIDECAR_SUFFIX)
}

/// Go: `sidecars`.
pub(crate) fn sidecar_files(dir: &Path) -> Vec<PathBuf> {
    files_with_suffix(dir, SIDECAR_SUFFIX)
}

/// Go: `flipByte`.
pub(crate) fn flip_byte(path: &Path, off: u64) {
    let f = OpenOptions::new()
        .read(true)
        .write(true)
        .open(path)
        .unwrap();
    let mut b = [0u8; 1];
    f.read_exact_at(&mut b, off).unwrap();
    b[0] ^= 0xff;
    f.write_all_at(&b, off).unwrap();
}

/// Go: `readSidecarFile`.
pub(crate) fn read_sidecar_file(path: &Path) -> (Vec<SidecarRec>, usize) {
    read_sidecar(&fs::read(path).unwrap(), true)
}

/// Go: `sidecarImage`.
pub(crate) fn sidecar_image(recs: &[SidecarRec]) -> Vec<u8> {
    let mut b = SIDECAR_MAGIC.to_vec();
    for r in recs {
        b.extend_from_slice(&r.encode());
    }
    b
}

/// Appends a complete footer to the directory's only active segment, as a
/// seal that crashed before its rename leaves it (Go: `crashSeal`).
pub(crate) fn crash_seal(dir: &Path) {
    let data = only_active(dir);
    let res = scan_active(&data).unwrap();
    let entries: Vec<IndexEntry> = res
        .index
        .iter()
        .map(|(k, loc)| IndexEntry {
            k: *k,
            off: loc.off,
            slen: loc.slen,
        })
        .collect();
    let footer = build_footer(res.size, &entries).unwrap();
    let mut b = fs::read(&data).unwrap();
    b.extend_from_slice(&footer);
    fs::write(&data, &b).unwrap();
}

/// A live predicate true for exactly the objects at `idx`, fit to move into a
/// thread (Go: `liveSet`).
pub(crate) fn live_at(
    objs: &[Object],
    idx: &[usize],
) -> impl Fn(Key) -> bool + Send + Sync + 'static {
    let live: std::collections::HashSet<Key> = idx.iter().map(|&i| objs[i].key).collect();
    move |k| live.contains(&k)
}

/// The options the compaction tests sweep with.
pub(crate) fn sweep_opts() -> super::CompactOpts {
    super::CompactOpts {
        min_dead_ratio: 0.1,
        ..super::CompactOpts::default()
    }
}

/// Kills the child process `pid` unless the sender it returns is dropped
/// within `limit`: a child that hangs must fail its test, not hang the suite
/// (`cargo test` has no timeout of its own).
pub(crate) fn watchdog(pid: u32, limit: std::time::Duration) -> std::sync::mpsc::Sender<()> {
    let (tx, rx) = std::sync::mpsc::channel::<()>();
    std::thread::spawn(move || {
        if rx.recv_timeout(limit) == Err(std::sync::mpsc::RecvTimeoutError::Timeout) {
            // SAFETY: kill takes a process id and a signal number.
            unsafe { libc::kill(pid as libc::pid_t, libc::SIGKILL) };
        }
    });
    tx
}
