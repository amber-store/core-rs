//! Traverses an Amber-Store CAS from a directory key and writes a PAX-format
//! tar of the filesystem tree. PAX is required to faithfully carry nanosecond
//! mtimes, extended attributes (`SCHILY.xattr.*`), long names, and device
//! nodes. Sockets cannot be archived and are skipped. The tree's root
//! directory itself is not emitted (its metadata is not stored); only its
//! descendants are.
//!
//! Byte compatibility: the output is identical to the Go implementation,
//! which delegates to Go's `archive/tar` writer. The PAX/USTAR write subset
//! of `archive/tar` (Go 1.26) is ported below as crate-private machinery —
//! ustar field fitting, PAX record selection/formatting/sorting, the
//! `PaxHeaders.0/<name>` extended-header naming, USTAR prefix splitting,
//! 512-byte framing and the two-zero-block trailer. See `port-notes/tar.md`
//! for the exact subset and the parts deliberately left out (GNU writer,
//! sparse files).

use std::collections::BTreeMap;
use std::fmt;
use std::io;

use crate::cbor;
use crate::fstree::{self, Entry, WalkError};
use crate::key::{self, Key, Type};

// POSIX file-type bits (Go uses golang.org/x/sys/unix; the values are
// universal and shared with the fstree entries that carry them).
const S_IFMT: u64 = 0o170000;
const S_IFDIR: u64 = 0o040000;
const S_IFREG: u64 = 0o100000;
const S_IFLNK: u64 = 0o120000;
const S_IFIFO: u64 = 0o010000;
const S_IFCHR: u64 = 0o020000;
const S_IFBLK: u64 = 0o060000;
const S_IFSOCK: u64 = 0o140000;

// ===========================================================================
// Public API
// ===========================================================================

/// Errors from [`write`], one variant per Go error site with the same
/// diagnostic text. `E` is the caller's getter error type.
#[derive(Debug)]
pub enum Error<E> {
    /// `tarexport: root <key> is not a directory object (type <type>)`
    NotDirectory {
        /// The rejected root key.
        key: Key,
    },
    /// A directory-walk failure from fstree (returned unwrapped, as in Go).
    Walk(WalkError<E>),
    /// `tarexport: refusing unsafe entry name "<name>"`
    UnsafeName {
        /// The refused entry name.
        name: Vec<u8>,
    },
    /// A content key that fails to parse (Go returns the key error bare).
    Key(key::Error),
    /// `tarexport: <name>: device entry missing [major, minor]`
    MissingRdev {
        /// The device entry's archive path.
        name: Vec<u8>,
    },
    /// `tarexport: <name>: unsupported file type <mode>`
    UnsupportedType {
        /// The entry's archive path.
        name: Vec<u8>,
        /// The unsupported `mode & S_IFMT` bits.
        mode: u64,
    },
    /// `tarexport: xattrs key for <name>: <err>`
    XattrsKey {
        /// The entry's archive path.
        name: Vec<u8>,
        /// The key parse failure.
        source: key::Error,
    },
    /// `tarexport: reading xattrs <key> for <name>: <err>`
    XattrsRead {
        /// The spilled XattrSet object's key.
        key: Key,
        /// The entry's archive path.
        name: Vec<u8>,
        /// The getter's error.
        source: E,
    },
    /// A xattr map decode failure (Go returns the cborx error bare).
    XattrsDecode(cbor::Error),
    /// `tarexport: <err>` — a content-streaming failure from
    /// [`fstree::write_content`].
    Content(WalkError<E>),
    /// `archive/tar: cannot encode header[: reasons]` — the entry metadata
    /// cannot be represented (unreachable for well-formed fstree entries).
    CannotEncodeHeader {
        /// The non-empty "why" fragments, joined with `"; and "` as Go does.
        reasons: Vec<String>,
    },
    /// `archive/tar: header field too long`
    FieldTooLong,
    /// `archive/tar: missed writing <n> bytes` — a regular file's streamed
    /// content was shorter than the length its key claims.
    MissedBytes {
        /// How many bytes were still owed.
        n: u64,
    },
    /// `archive/tar: invalid tar header` — a PAX record failed validation at
    /// format time (unreachable: records are validated beforehand).
    InvalidHeader,
    /// An I/O failure on the output writer.
    Io(io::Error),
}

impl<E: fmt::Display> fmt::Display for Error<E> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::NotDirectory { key } => write!(
                f,
                "tarexport: root {key} is not a directory object (type {})",
                key.type_()
            ),
            Error::Walk(e) => e.fmt(f),
            Error::UnsafeName { name } => {
                write!(f, "tarexport: refusing unsafe entry name {}", GoQuote(name))
            }
            Error::Key(e) => e.fmt(f),
            Error::MissingRdev { name } => write!(
                f,
                "tarexport: {}: device entry missing [major, minor]",
                String::from_utf8_lossy(name)
            ),
            Error::UnsupportedType { name, mode } => {
                // Go renders the mode with %#o ("0" prefix, bare "0" for 0).
                write!(
                    f,
                    "tarexport: {}: unsupported file type ",
                    String::from_utf8_lossy(name)
                )?;
                if *mode == 0 {
                    write!(f, "0")
                } else {
                    write!(f, "0{mode:o}")
                }
            }
            Error::XattrsKey { name, source } => write!(
                f,
                "tarexport: xattrs key for {}: {source}",
                String::from_utf8_lossy(name)
            ),
            Error::XattrsRead { key, name, source } => write!(
                f,
                "tarexport: reading xattrs {key} for {}: {source}",
                String::from_utf8_lossy(name)
            ),
            Error::XattrsDecode(e) => e.fmt(f),
            Error::Content(e) => write!(f, "tarexport: {e}"),
            Error::CannotEncodeHeader { reasons } => {
                write!(f, "archive/tar: cannot encode header")?;
                let parts: Vec<&str> = reasons
                    .iter()
                    .filter(|s| !s.is_empty())
                    .map(String::as_str)
                    .collect();
                if !parts.is_empty() {
                    write!(f, ": {}", parts.join("; and "))?;
                }
                Ok(())
            }
            Error::FieldTooLong => write!(f, "archive/tar: header field too long"),
            Error::MissedBytes { n } => write!(f, "archive/tar: missed writing {n} bytes"),
            Error::InvalidHeader => write!(f, "archive/tar: invalid tar header"),
            Error::Io(e) => e.fmt(f),
        }
    }
}

impl<E: std::error::Error + 'static> std::error::Error for Error<E> {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Error::Walk(e) | Error::Content(e) => Some(e),
            Error::Key(e) | Error::XattrsKey { source: e, .. } => Some(e),
            Error::XattrsRead { source, .. } => Some(source),
            Error::XattrsDecode(e) => Some(e),
            Error::Io(e) => Some(e),
            _ => None,
        }
    }
}

impl<E> Error<E> {
    fn from_tw(e: TwError) -> Error<E> {
        match e {
            TwError::Io(e) => Error::Io(e),
            TwError::FieldTooLong => Error::FieldTooLong,
            TwError::Missed(n) => Error::MissedBytes { n },
            TwError::Header(reasons) => Error::CannotEncodeHeader { reasons },
            TwError::BadRecord => Error::InvalidHeader,
        }
    }
}

/// Streams a PAX tar of the directory tree rooted at `root` to `w`. `root`
/// must be a directory object (DirLeaf or DirNode). `get` fetches the bytes
/// stored under a key.
pub fn write<W, G, E>(w: &mut W, root: Key, get: G) -> Result<(), Error<E>>
where
    W: io::Write + ?Sized,
    G: FnMut(Key) -> Result<Vec<u8>, E>,
{
    if root.type_() != Type::DirLeaf && root.type_() != Type::DirNode {
        return Err(Error::NotDirectory { key: root });
    }
    let mut e = Exporter {
        tw: TarWriter::new(w),
        get,
    };
    e.dir(root, b"")?;
    e.tw.close().map_err(Error::from_tw)
}

struct Exporter<'a, W: io::Write + ?Sized, G> {
    tw: TarWriter<&'a mut W>,
    get: G,
}

impl<W, G, E> Exporter<'_, W, G>
where
    W: io::Write + ?Sized,
    G: FnMut(Key) -> Result<Vec<u8>, E>,
{
    /// Writes every entry of the directory object `dir_key`, prefixing names
    /// with `prefix` (the path of the directory relative to the export root,
    /// empty for the root).
    fn dir(&mut self, dir_key: Key, prefix: &[u8]) -> Result<(), Error<E>> {
        let entries = fstree::collect_entries(dir_key, &mut self.get).map_err(Error::Walk)?;
        for ent in entries {
            self.entry(prefix, ent)?;
        }
        Ok(())
    }

    fn entry(&mut self, prefix: &[u8], ent: Entry) -> Result<(), Error<E>> {
        let comp = ent.name.as_slice();
        if comp.is_empty() || comp == b"." || comp == b".." || comp.contains(&b'/') {
            return Err(Error::UnsafeName {
                name: comp.to_vec(),
            });
        }
        let name = path_join(&[prefix, comp]);
        let mut hdr = TarHeader {
            name: name.clone(),
            mode: (ent.mode & 0o7777) as i64,
            uid: ent.uid as i64,
            gid: ent.gid as i64,
            mod_time: Some(PaxTime::unix(0, ent.mtime)),
            format: F_PAX,
            ..TarHeader::default()
        };
        self.set_xattrs(&mut hdr, &ent)?;

        match ent.mode & S_IFMT {
            S_IFDIR => {
                hdr.typeflag = TYPE_DIR;
                hdr.name = [name.as_slice(), b"/"].concat();
                self.tw.write_header(&hdr).map_err(Error::from_tw)?;
                let ck = Key::parse(&ent.content_key).map_err(Error::Key)?;
                self.dir(ck, &name)
            }
            S_IFREG => {
                let ck = Key::parse(&ent.content_key).map_err(Error::Key)?;
                hdr.typeflag = TYPE_REG;
                // FileNode/Blob length == content byte count.
                hdr.size = ck.length() as i64;
                self.tw.write_header(&hdr).map_err(Error::from_tw)?;
                self.write_content(ck)
            }
            S_IFLNK => {
                hdr.typeflag = TYPE_SYMLINK;
                hdr.linkname = ent.link_target.clone();
                self.tw.write_header(&hdr).map_err(Error::from_tw)
            }
            S_IFIFO => {
                hdr.typeflag = TYPE_FIFO;
                self.tw.write_header(&hdr).map_err(Error::from_tw)
            }
            S_IFCHR | S_IFBLK => {
                if ent.rdev.len() != 2 {
                    return Err(Error::MissingRdev { name });
                }
                hdr.typeflag = if ent.mode & S_IFMT == S_IFCHR {
                    TYPE_CHAR
                } else {
                    TYPE_BLOCK
                };
                hdr.devmajor = ent.rdev[0] as i64;
                hdr.devminor = ent.rdev[1] as i64;
                self.tw.write_header(&hdr).map_err(Error::from_tw)
            }
            S_IFSOCK => {
                // Sockets cannot be archived; skip (consistent with restore).
                Ok(())
            }
            m => Err(Error::UnsupportedType { name, mode: m }),
        }
    }

    /// Writes the bytes addressed by `k` to the current tar member, descending
    /// FileNode index levels and concatenating Blob leaves in order.
    fn write_content(&mut self, k: Key) -> Result<(), Error<E>> {
        fstree::write_content(&mut self.tw, k, &mut self.get).map_err(Error::Content)
    }

    /// Decodes an entry's extended attributes (inline or spilled) into the
    /// header's PAX records under the `SCHILY.xattr.*` namespace.
    fn set_xattrs(&mut self, hdr: &mut TarHeader, ent: &Entry) -> Result<(), Error<E>> {
        let m = if !ent.xattrs_in.is_empty() {
            cbor::decode_xattrs(&ent.xattrs_in).map_err(Error::XattrsDecode)?
        } else if ent.xattrs_key.len() == key::SIZE {
            let xk = Key::parse(&ent.xattrs_key).map_err(|source| Error::XattrsKey {
                name: hdr.name.clone(),
                source,
            })?;
            let data = (self.get)(xk).map_err(|source| Error::XattrsRead {
                key: xk,
                name: hdr.name.clone(),
                source,
            })?;
            cbor::decode_xattrs(&data).map_err(Error::XattrsDecode)?
        } else {
            return Ok(());
        };
        if m.is_empty() {
            return Ok(());
        }
        for (k, v) in m {
            hdr.pax_records
                .insert([b"SCHILY.xattr.".as_slice(), &k].concat(), v);
        }
        Ok(())
    }
}

// ===========================================================================
// Crate-private tar framing (port of the Go archive/tar PAX/USTAR write
// subset; the layout constants and small helpers are shared with the
// tarextract reader)
// ===========================================================================

pub(crate) const BLOCK_SIZE: usize = 512;
pub(crate) const NAME_SIZE: usize = 100;
const PREFIX_SIZE: usize = 155;
/// Max length of a special file (PAX header, GNU long name or link).
pub(crate) const MAX_SPECIAL_FILE_SIZE: usize = 1 << 20;

// Type flags for TarHeader::typeflag (Go archive/tar constants).
pub(crate) const TYPE_REG: u8 = b'0';
pub(crate) const TYPE_REG_A: u8 = 0; // deprecated; promoted on write and read
pub(crate) const TYPE_LINK: u8 = b'1';
pub(crate) const TYPE_SYMLINK: u8 = b'2';
pub(crate) const TYPE_CHAR: u8 = b'3';
pub(crate) const TYPE_BLOCK: u8 = b'4';
pub(crate) const TYPE_DIR: u8 = b'5';
pub(crate) const TYPE_FIFO: u8 = b'6';
pub(crate) const TYPE_XHEADER: u8 = b'x';
pub(crate) const TYPE_XGLOBAL_HEADER: u8 = b'g';
pub(crate) const TYPE_GNU_SPARSE: u8 = b'S';
pub(crate) const TYPE_GNU_LONGNAME: u8 = b'L';
pub(crate) const TYPE_GNU_LONGLINK: u8 = b'K';

// Format bit set (Go's Format constants; FormatUnknown == 0).
pub(crate) const F_UNKNOWN: u8 = 0;
pub(crate) const F_V7: u8 = 1;
pub(crate) const F_USTAR: u8 = 2;
pub(crate) const F_PAX: u8 = 4;
pub(crate) const F_GNU: u8 = 8;
pub(crate) const F_STAR: u8 = 16;

// Header block field positions as (offset, length).
pub(crate) const V7_NAME: (usize, usize) = (0, 100);
pub(crate) const V7_MODE: (usize, usize) = (100, 8);
pub(crate) const V7_UID: (usize, usize) = (108, 8);
pub(crate) const V7_GID: (usize, usize) = (116, 8);
pub(crate) const V7_SIZE: (usize, usize) = (124, 12);
pub(crate) const V7_MTIME: (usize, usize) = (136, 12);
pub(crate) const V7_CHKSUM: (usize, usize) = (148, 8);
pub(crate) const V7_TYPEFLAG: usize = 156;
pub(crate) const V7_LINKNAME: (usize, usize) = (157, 100);
pub(crate) const USTAR_MAGIC: (usize, usize) = (257, 6);
pub(crate) const USTAR_VERSION: (usize, usize) = (263, 2);
pub(crate) const USTAR_UNAME: (usize, usize) = (265, 32);
pub(crate) const USTAR_GNAME: (usize, usize) = (297, 32);
pub(crate) const USTAR_DEVMAJOR: (usize, usize) = (329, 8);
pub(crate) const USTAR_DEVMINOR: (usize, usize) = (337, 8);
pub(crate) const USTAR_PREFIX: (usize, usize) = (345, 155);
pub(crate) const STAR_PREFIX: (usize, usize) = (345, 131);
pub(crate) const STAR_ATIME: (usize, usize) = (476, 12);
pub(crate) const STAR_CTIME: (usize, usize) = (488, 12);
pub(crate) const STAR_TRAILER: (usize, usize) = (508, 4);
pub(crate) const GNU_ATIME: (usize, usize) = (345, 12);
pub(crate) const GNU_CTIME: (usize, usize) = (357, 12);

/// Borrows a header-block field.
pub(crate) fn fld(blk: &[u8; BLOCK_SIZE], r: (usize, usize)) -> &[u8] {
    &blk[r.0..r.0 + r.1]
}

fn fld_mut(blk: &mut [u8; BLOCK_SIZE], r: (usize, usize)) -> &mut [u8] {
    &mut blk[r.0..r.0 + r.1]
}

/// Reports whether the type flag is header-only (no data body even if a size
/// is present).
pub(crate) fn is_header_only(flag: u8) -> bool {
    matches!(
        flag,
        TYPE_LINK | TYPE_SYMLINK | TYPE_CHAR | TYPE_BLOCK | TYPE_DIR | TYPE_FIFO
    )
}

/// The number of bytes needed to pad `offset` up to the next 512-byte block
/// edge (`0 <= n < 512`).
pub(crate) fn block_padding(offset: i64) -> i64 {
    -offset & (BLOCK_SIZE as i64 - 1)
}

/// POSIX-specified unsigned and Sun-signed header checksums; the checksum
/// field itself counts as spaces.
pub(crate) fn compute_checksum(blk: &[u8; BLOCK_SIZE]) -> (i64, i64) {
    let (mut unsigned, mut signed) = (0i64, 0i64);
    for (i, &c) in blk.iter().enumerate() {
        let c = if (V7_CHKSUM.0..V7_CHKSUM.0 + V7_CHKSUM.1).contains(&i) {
            b' '
        } else {
            c
        };
        unsigned += c as i64;
        signed += (c as i8) as i64;
    }
    (unsigned, signed)
}

/// A Unix time as PAX represents it: whole seconds plus a normalized
/// nanosecond offset in `0..1e9` (Go `time.Time` restricted to what the tar
/// code paths use). `None` at a use site stands for Go's zero `time.Time`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct PaxTime {
    pub(crate) sec: i64,
    pub(crate) nsec: u32,
}

impl PaxTime {
    /// Go `time.Unix(sec, nsec)`: normalizes nsec into `0..1e9`, borrowing
    /// from the seconds.
    pub(crate) fn unix(sec: i64, nsec: i64) -> PaxTime {
        let (mut sec, mut nsec) = (sec, nsec);
        if !(0..1_000_000_000).contains(&nsec) {
            let n = nsec / 1_000_000_000;
            sec = sec.wrapping_add(n);
            nsec -= n * 1_000_000_000;
            if nsec < 0 {
                nsec += 1_000_000_000;
                sec = sec.wrapping_sub(1);
            }
        }
        PaxTime {
            sec,
            nsec: nsec as u32,
        }
    }

    /// Go `Time.UnixNano()` (wraps on overflow exactly as Go's int64 does).
    pub(crate) fn unix_nanos(self) -> i64 {
        self.sec
            .wrapping_mul(1_000_000_000)
            .wrapping_add(self.nsec as i64)
    }

    /// Go `Time.Round(time.Second)` for Unix-representable times: half away
    /// from zero on the sub-second component.
    fn round_to_second(self) -> PaxTime {
        PaxTime {
            sec: if self.nsec >= 500_000_000 {
                self.sec.wrapping_add(1)
            } else {
                self.sec
            },
            nsec: 0,
        }
    }
}

/// PAX extended-header records, sorted by key bytes (Go's sorted-string
/// order).
pub(crate) type PaxRecords = BTreeMap<Vec<u8>, Vec<u8>>;

/// One tar header (Go `tar.Header` restricted to the fields the export and
/// extract paths use; strings are raw bytes, as Go strings are). The
/// deprecated `Xattrs` field is not carried — extended attributes travel in
/// `pax_records` under `SCHILY.xattr.*`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct TarHeader {
    pub(crate) typeflag: u8,
    pub(crate) name: Vec<u8>,
    pub(crate) linkname: Vec<u8>,
    pub(crate) size: i64,
    pub(crate) mode: i64,
    pub(crate) uid: i64,
    pub(crate) gid: i64,
    pub(crate) uname: Vec<u8>,
    pub(crate) gname: Vec<u8>,
    pub(crate) mod_time: Option<PaxTime>,
    pub(crate) access_time: Option<PaxTime>,
    pub(crate) change_time: Option<PaxTime>,
    pub(crate) devmajor: i64,
    pub(crate) devminor: i64,
    pub(crate) pax_records: PaxRecords,
    pub(crate) format: u8,
}

/// Internal tar-writer errors, one per Go `archive/tar` writer error value.
#[derive(Debug)]
pub(crate) enum TwError {
    /// An underlying I/O failure.
    Io(io::Error),
    /// `ErrFieldTooLong`.
    FieldTooLong,
    /// `archive/tar: missed writing <n> bytes` (flush with unwritten data).
    Missed(u64),
    /// `headerError` — the "why" fragments; empties are filtered on display.
    Header(Vec<String>),
    /// `ErrHeader` out of `formatPAXRecord`.
    BadRecord,
}

// --- strconv.go subset ---

/// Go `hasNUL`.
pub(crate) fn has_nul(s: &[u8]) -> bool {
    s.contains(&0)
}

/// Go `isASCII`: no NUL and no byte >= 0x80.
pub(crate) fn is_ascii_str(s: &[u8]) -> bool {
    s.iter().all(|&c| c != 0 && c < 0x80)
}

/// Go `toASCII`: best-effort conversion dropping invalid characters. Go
/// iterates runes and drops runes >= U+0080 and NULs; since every byte of a
/// multi-byte or invalid UTF-8 sequence is >= 0x80, this equals a byte
/// filter.
pub(crate) fn to_ascii(s: &[u8]) -> Vec<u8> {
    s.iter().copied().filter(|&c| c != 0 && c < 0x80).collect()
}

/// Reports whether `x` fits in an `n`-byte field using base-256 (GNU binary)
/// encoding.
fn fits_in_base256(n: usize, x: i64) -> bool {
    if n >= 9 {
        return true;
    }
    let bin_bits = (n as u32 - 1) * 8;
    x >= -(1i64 << bin_bits) && x < 1i64 << bin_bits
}

/// Reports whether `x` fits in an `n`-byte field using octal encoding with
/// the appropriate NUL terminator.
fn fits_in_octal(n: usize, x: i64) -> bool {
    if x < 0 {
        return false;
    }
    if n >= 22 {
        return true;
    }
    let oct_bits = (n as u32 - 1) * 3;
    x < 1i64 << oct_bits
}

/// Field formatter with Go's sticky `ErrFieldTooLong`.
#[derive(Default)]
struct Formatter {
    field_too_long: bool,
}

impl Formatter {
    /// Copies `s` into `b`, NUL-terminating if possible (Go `formatString`,
    /// including the buggy-reader trailing-slash fix-up).
    fn format_string(&mut self, b: &mut [u8], s: &[u8]) {
        if s.len() > b.len() {
            self.field_too_long = true;
        }
        let n = s.len().min(b.len());
        b[..n].copy_from_slice(&s[..n]);
        if s.len() < b.len() {
            b[s.len()] = 0;
        }

        // Some buggy readers treat regular files with a trailing slash in the
        // V7 path field as a directory even though the full path recorded
        // elsewhere (e.g., via PAX record) contains no trailing slash.
        if s.len() > b.len() && b[b.len() - 1] == b'/' {
            let mut k = b.len() - 1; // len(TrimRight(s[:len(b)-1], "/"))
            while k > 0 && s[k - 1] == b'/' {
                k -= 1;
            }
            b[k] = 0; // Replace trailing slash with NUL terminator
        }
    }

    /// Encodes `x` into `b` in octal, zero-padded, NUL-terminated (Go
    /// `formatOctal`; out-of-range writes zero and sets the sticky error).
    fn format_octal(&mut self, b: &mut [u8], x: i64) {
        let mut x = x;
        if !fits_in_octal(b.len(), x) {
            x = 0; // Last resort, just write zero.
            self.field_too_long = true;
        }
        let mut s = format!("{x:o}");
        if b.len() > s.len() + 1 {
            // Add leading zeros, but leave room for a NUL.
            s = "0".repeat(b.len() - s.len() - 1) + &s;
        }
        self.format_string(b, s.as_bytes());
    }
}

/// Go `formatPAXTime`: `%d[.%09d]` with trailing zeros trimmed; capable of
/// negative timestamps.
pub(crate) fn format_pax_time(ts: PaxTime) -> Vec<u8> {
    let (mut secs, mut nsecs) = (ts.sec, ts.nsec as i64);
    if nsecs == 0 {
        return secs.to_string().into_bytes();
    }

    // If seconds is negative, then perform correction.
    let mut sign = "";
    if secs < 0 {
        sign = "-"; // Remember sign
        secs = -(secs + 1); // Add a second to secs
        nsecs = -(nsecs - 1_000_000_000); // Take that second away from nsecs
    }
    format!("{sign}{secs}.{nsecs:09}")
        .trim_end_matches('0')
        .as_bytes()
        .to_vec()
}

/// Go `validPAXRecord`: keys must be non-empty, `=`-free, and NUL-free; the
/// values of the four USTAR string keys are the NUL-checked ones instead.
pub(crate) fn valid_pax_record(k: &[u8], v: &[u8]) -> bool {
    if k.is_empty() || k.contains(&b'=') {
        return false;
    }
    match k {
        b"path" | b"linkpath" | b"uname" | b"gname" => !has_nul(v),
        _ => !has_nul(k),
    }
}

/// Go `formatPAXRecord`: `"%d %s=%s\n"` where the length prefix includes
/// itself (with the one-round adjustment).
pub(crate) fn format_pax_record(k: &[u8], v: &[u8]) -> Option<Vec<u8>> {
    if !valid_pax_record(k, v) {
        return None;
    }
    const PADDING: usize = 3; // Extra padding for ' ', '=', and '\n'
    let mut size = k.len() + v.len() + PADDING;
    size += size.to_string().len();
    let rec = |size: usize| [size.to_string().as_bytes(), b" ", k, b"=", v, b"\n"].concat();
    let mut record = rec(size);

    // Final adjustment if adding size field increased the record size.
    if record.len() != size {
        size = record.len();
        record = rec(size);
    }
    Some(record)
}

// --- path helpers (Go path.Clean / Join / Split, bytewise) ---

/// Go `path.Clean`.
pub(crate) fn path_clean(path: &[u8]) -> Vec<u8> {
    if path.is_empty() {
        return b".".to_vec();
    }
    let rooted = path[0] == b'/';
    let n = path.len();
    let mut out: Vec<u8> = Vec::with_capacity(n);
    let mut r = 0usize;
    let mut dotdot = 0usize;
    if rooted {
        out.push(b'/');
        r = 1;
        dotdot = 1;
    }
    while r < n {
        if path[r] == b'/' {
            // Empty path element.
            r += 1;
        } else if path[r] == b'.' && (r + 1 == n || path[r + 1] == b'/') {
            // "." element.
            r += 1;
        } else if path[r] == b'.'
            && r + 1 < n
            && path[r + 1] == b'.'
            && (r + 2 == n || path[r + 2] == b'/')
        {
            // ".." element.
            r += 2;
            if out.len() > dotdot {
                // Can backtrack: cut back to (and including) the last '/'.
                let mut w = out.len() - 1;
                while w > dotdot && out[w] != b'/' {
                    w -= 1;
                }
                out.truncate(w);
            } else if !rooted {
                // Cannot backtrack, but not rooted, so append "..".
                if !out.is_empty() {
                    out.push(b'/');
                }
                out.extend_from_slice(b"..");
                dotdot = out.len();
            }
        } else {
            // Real path element; add slash if needed.
            if (rooted && out.len() != 1) || (!rooted && !out.is_empty()) {
                out.push(b'/');
            }
            while r < n && path[r] != b'/' {
                out.push(path[r]);
                r += 1;
            }
        }
    }
    if out.is_empty() {
        return b".".to_vec();
    }
    out
}

/// Go `path.Join`: joins from the first non-empty element on (later empty
/// elements still contribute separators) and cleans the result.
pub(crate) fn path_join(elems: &[&[u8]]) -> Vec<u8> {
    for (i, e) in elems.iter().enumerate() {
        if !e.is_empty() {
            let mut joined = Vec::new();
            for (j, part) in elems[i..].iter().enumerate() {
                if j > 0 {
                    joined.push(b'/');
                }
                joined.extend_from_slice(part);
            }
            return path_clean(&joined);
        }
    }
    Vec::new()
}

/// Go `path.Split`: splits after the final slash.
pub(crate) fn path_split(p: &[u8]) -> (&[u8], &[u8]) {
    match p.iter().rposition(|&b| b == b'/') {
        Some(i) => (&p[..i + 1], &p[i + 1..]),
        None => (&p[..0], p),
    }
}

/// Renders bytes roughly as Go's `%q` (used in error messages only; exact
/// for ASCII input, best-effort for non-ASCII — see port-notes/tar.md).
pub(crate) struct GoQuote<'a>(pub(crate) &'a [u8]);

impl fmt::Display for GoQuote<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "\"")?;
        let mut rest = self.0;
        while !rest.is_empty() {
            match std::str::from_utf8(rest) {
                Ok(s) => {
                    for c in s.chars() {
                        fmt_quoted_char(f, c)?;
                    }
                    rest = b"";
                }
                Err(e) => {
                    let (valid, bad) = rest.split_at(e.valid_up_to());
                    for c in std::str::from_utf8(valid)
                        .expect("validated prefix")
                        .chars()
                    {
                        fmt_quoted_char(f, c)?;
                    }
                    write!(f, "\\x{:02x}", bad[0])?;
                    rest = &bad[1..];
                }
            }
        }
        write!(f, "\"")
    }
}

fn fmt_quoted_char(f: &mut fmt::Formatter<'_>, c: char) -> fmt::Result {
    match c {
        '"' => write!(f, "\\\""),
        '\\' => write!(f, "\\\\"),
        '\n' => write!(f, "\\n"),
        '\r' => write!(f, "\\r"),
        '\t' => write!(f, "\\t"),
        '\x07' => write!(f, "\\a"),
        '\x08' => write!(f, "\\b"),
        '\x0b' => write!(f, "\\v"),
        '\x0c' => write!(f, "\\f"),
        c if (c as u32) < 0x20 || c as u32 == 0x7f => write!(f, "\\x{:02x}", c as u32),
        c => write!(f, "{c}"),
    }
}

// --- writer.go subset ---

/// Go `splitUSTARPath`: splits a path according to USTAR prefix and suffix
/// rules, or `None` if unsplittable.
pub(crate) fn split_ustar_path(name: &[u8]) -> Option<(&[u8], &[u8])> {
    let mut length = name.len();
    if length <= NAME_SIZE || !is_ascii_str(name) {
        return None;
    }
    if length > PREFIX_SIZE + 1 {
        length = PREFIX_SIZE + 1;
    } else if name[length - 1] == b'/' {
        length -= 1;
    }

    let i = match name[..length].iter().rposition(|&b| b == b'/') {
        None | Some(0) => return None, // Go: i <= 0
        Some(i) => i,
    };
    let nlen = name.len() - i - 1; // nlen is length of suffix
    let plen = i; // plen is length of prefix
    if nlen > NAME_SIZE || nlen == 0 || plen > PREFIX_SIZE {
        return None;
    }
    Some((&name[..i], &name[i + 1..]))
}

/// Writes the magic values for `format` into the block and updates the
/// checksum (Go `block.setFormat`; the V7 and STAR arms are never written by
/// this writer).
fn set_format(blk: &mut [u8; BLOCK_SIZE], format: u8) {
    if format & F_GNU != 0 {
        fld_mut(blk, USTAR_MAGIC).copy_from_slice(b"ustar ");
        fld_mut(blk, USTAR_VERSION).copy_from_slice(b" \0");
    } else if format & (F_USTAR | F_PAX) != 0 {
        fld_mut(blk, USTAR_MAGIC).copy_from_slice(b"ustar\0");
        fld_mut(blk, USTAR_VERSION).copy_from_slice(b"00");
    } else {
        unreachable!("setFormat: invalid format");
    }

    // The checksum field is special: terminated by a NUL then a space.
    let (chksum, _) = compute_checksum(blk); // Possible values are 256..128776
    let mut f = Formatter::default();
    f.format_octal(&mut blk[V7_CHKSUM.0..V7_CHKSUM.0 + 7], chksum); // Never fails: 128776 < 262143
    blk[V7_CHKSUM.0 + 7] = b' ';
}

/// The PAX keys with built-in `tar.Header` support.
const BASIC_KEYS: [&[u8]; 10] = [
    b"path",
    b"linkpath",
    b"size",
    b"uid",
    b"gid",
    b"uname",
    b"gname",
    b"mtime",
    b"atime",
    b"ctime",
];

fn is_basic_key(k: &[u8]) -> bool {
    BASIC_KEYS.contains(&k)
}

/// State threaded through the `allowed_formats` field checks (the captured
/// variables of Go's closures).
struct FmtCheck {
    format: u8,
    pax: PaxRecords,
    why_no_ustar: String,
    why_no_pax: String,
    why_no_gnu: String,
    prefer_pax: bool,
}

impl FmtCheck {
    fn verify_string(
        &mut self,
        records: &PaxRecords,
        s: &[u8],
        size: usize,
        name: &str,
        pax_key: &[u8],
    ) {
        // The NUL-terminator is optional for path and linkpath; neither GNU
        // nor BSD tar checks uname/gname for it.
        let too_long = s.len() > size;
        let allow_long_gnu = pax_key == b"path" || pax_key == b"linkpath";
        if has_nul(s) || (too_long && !allow_long_gnu) {
            self.why_no_gnu = format!("GNU cannot encode {name}={}", GoQuote(s));
            self.format &= !F_GNU;
        }
        if !is_ascii_str(s) || too_long {
            let can_split_ustar = pax_key == b"path";
            if !can_split_ustar || split_ustar_path(s).is_none() {
                self.why_no_ustar = format!("USTAR cannot encode {name}={}", GoQuote(s));
                self.format &= !F_USTAR;
            }
            if pax_key.is_empty() {
                self.why_no_pax = format!("PAX cannot encode {name}={}", GoQuote(s));
                self.format &= !F_PAX;
            } else {
                self.pax.insert(pax_key.to_vec(), s.to_vec());
            }
        }
        if records.get(pax_key).map(Vec::as_slice) == Some(s) {
            self.pax.insert(pax_key.to_vec(), s.to_vec());
        }
    }

    fn verify_numeric(
        &mut self,
        records: &PaxRecords,
        n: i64,
        size: usize,
        name: &str,
        pax_key: &[u8],
    ) {
        if !fits_in_base256(size, n) {
            self.why_no_gnu = format!("GNU cannot encode {name}={n}");
            self.format &= !F_GNU;
        }
        if !fits_in_octal(size, n) {
            self.why_no_ustar = format!("USTAR cannot encode {name}={n}");
            self.format &= !F_USTAR;
            if pax_key.is_empty() {
                self.why_no_pax = format!("PAX cannot encode {name}={n}");
                self.format &= !F_PAX;
            } else {
                self.pax
                    .insert(pax_key.to_vec(), n.to_string().into_bytes());
            }
        }
        let want = n.to_string().into_bytes();
        if records.get(pax_key).map(Vec::as_slice) == Some(want.as_slice()) {
            self.pax.insert(pax_key.to_vec(), want);
        }
    }

    fn verify_time(
        &mut self,
        records: &PaxRecords,
        ts: Option<PaxTime>,
        size: usize,
        name: &str,
        pax_key: &[u8],
    ) {
        let Some(ts) = ts else {
            return; // The zero time is always okay.
        };
        // Go renders the time with %v in these messages; the PAX form is
        // used here instead (error text only, see port-notes/tar.md).
        let render = || String::from_utf8_lossy(&format_pax_time(ts)).into_owned();
        if !fits_in_base256(size, ts.sec) {
            self.why_no_gnu = format!("GNU cannot encode {name}={}", render());
            self.format &= !F_GNU;
        }
        let is_mtime = pax_key == b"mtime";
        let fits_octal = fits_in_octal(size, ts.sec);
        // Go: (isMtime && !fitsOctal) || !isMtime.
        if !(is_mtime && fits_octal) {
            self.why_no_ustar = format!("USTAR cannot encode {name}={}", render());
            self.format &= !F_USTAR;
        }
        let needs_nano = ts.nsec != 0;
        if !is_mtime || !fits_octal || needs_nano {
            self.prefer_pax = true; // USTAR may truncate sub-second measurements
            if pax_key.is_empty() {
                self.why_no_pax = format!("PAX cannot encode {name}={}", render());
                self.format &= !F_PAX;
            } else {
                self.pax.insert(pax_key.to_vec(), format_pax_time(ts));
            }
        }
        let want = format_pax_time(ts);
        if records.get(pax_key).map(Vec::as_slice) == Some(want.as_slice()) {
            self.pax.insert(pax_key.to_vec(), want);
        }
    }
}

/// Go `Header.allowedFormats`: which formats can encode this header, plus
/// the PAX records for the fields that need them. GNU remains tracked so
/// that exclusion reasons and format masks match Go bit-for-bit even though
/// the GNU writer itself is not ported.
fn allowed_formats(hdr: &TarHeader) -> Result<(u8, PaxRecords), TwError> {
    let mut c = FmtCheck {
        format: F_USTAR | F_PAX | F_GNU,
        pax: BTreeMap::new(),
        why_no_ustar: String::new(),
        why_no_pax: String::new(),
        why_no_gnu: String::new(),
        prefer_pax: false,
    };
    let mut why_only_pax = String::new();
    let why_only_gnu = String::new(); // sparse files (its only source) are not ported

    // Check basic fields.
    let r = &hdr.pax_records;
    c.verify_string(r, &hdr.name, V7_NAME.1, "Name", b"path");
    c.verify_string(r, &hdr.linkname, V7_LINKNAME.1, "Linkname", b"linkpath");
    c.verify_string(r, &hdr.uname, USTAR_UNAME.1, "Uname", b"uname");
    c.verify_string(r, &hdr.gname, USTAR_GNAME.1, "Gname", b"gname");
    c.verify_numeric(r, hdr.mode, V7_MODE.1, "Mode", b"");
    c.verify_numeric(r, hdr.uid, V7_UID.1, "Uid", b"uid");
    c.verify_numeric(r, hdr.gid, V7_GID.1, "Gid", b"gid");
    c.verify_numeric(r, hdr.size, V7_SIZE.1, "Size", b"size");
    c.verify_numeric(r, hdr.devmajor, USTAR_DEVMAJOR.1, "Devmajor", b"");
    c.verify_numeric(r, hdr.devminor, USTAR_DEVMINOR.1, "Devminor", b"");
    c.verify_time(r, hdr.mod_time, V7_MTIME.1, "ModTime", b"mtime");
    c.verify_time(r, hdr.access_time, GNU_ATIME.1, "AccessTime", b"atime");
    c.verify_time(r, hdr.change_time, GNU_CTIME.1, "ChangeTime", b"ctime");

    // Check for header-only types. (TypeLink and TypeSymlink are excluded
    // from the trailing-slash check since they may reference directories.)
    match hdr.typeflag {
        TYPE_REG | TYPE_CHAR | TYPE_BLOCK | TYPE_FIFO | TYPE_GNU_SPARSE
            if hdr.name.ends_with(b"/") =>
        {
            return Err(TwError::Header(vec![
                "filename may not have trailing slash".into(),
            ]));
        }
        TYPE_XHEADER | TYPE_GNU_LONGNAME | TYPE_GNU_LONGLINK => {
            return Err(TwError::Header(vec![
                "cannot manually encode TypeXHeader, TypeGNULongName, or TypeGNULongLink headers"
                    .into(),
            ]));
        }
        TYPE_XGLOBAL_HEADER => {
            // Go additionally requires that only Name/Typeflag/Xattrs/
            // PAXRecords/Format be set; this crate never writes global
            // headers, so that reflect.DeepEqual guard is not ported.
            why_only_pax = "only PAX supports TypeXGlobalHeader".into();
            c.format &= F_PAX;
        }
        _ => {}
    }
    if !is_header_only(hdr.typeflag) && hdr.size < 0 {
        return Err(TwError::Header(vec![
            "negative size on header-only type".into(),
        ]));
    }

    // Check PAX records. (The deprecated Xattrs field does not exist here;
    // extended attributes always arrive as SCHILY.xattr.* records.)
    if !hdr.pax_records.is_empty() {
        for (k, v) in &hdr.pax_records {
            if c.pax.contains_key(k) {
                continue; // Do not overwrite existing records
            }
            if hdr.typeflag == TYPE_XGLOBAL_HEADER {
                c.pax.insert(k.clone(), v.clone()); // Copy all records
            } else if !is_basic_key(k) && !k.starts_with(b"GNU.sparse.") {
                c.pax.insert(k.clone(), v.clone()); // Ignore local records that may conflict
            }
        }
        why_only_pax = "only PAX supports PAXRecords".into();
        c.format &= F_PAX;
    }
    for (k, v) in &c.pax {
        if !valid_pax_record(k, v) {
            return Err(TwError::Header(vec![format!(
                "invalid PAX record: {}",
                GoQuote(&[k.as_slice(), b" = ", v].concat())
            )]));
        }
    }

    // Check desired format.
    if hdr.format != F_UNKNOWN {
        let mut want = hdr.format;
        if want & F_PAX != 0 && !c.prefer_pax {
            want |= F_USTAR; // PAX implies USTAR allowed too
        }
        c.format &= want; // Set union of formats allowed and format wanted
    }
    if c.format == F_UNKNOWN {
        let reasons = match hdr.format {
            F_USTAR => vec![
                "Format specifies USTAR".into(),
                c.why_no_ustar,
                why_only_pax,
                why_only_gnu,
            ],
            F_PAX => vec!["Format specifies PAX".into(), c.why_no_pax, why_only_gnu],
            F_GNU => vec!["Format specifies GNU".into(), c.why_no_gnu, why_only_pax],
            _ => vec![
                c.why_no_ustar,
                c.why_no_pax,
                c.why_no_gnu,
                why_only_pax,
                why_only_gnu,
            ],
        };
        return Err(TwError::Header(reasons));
    }
    Ok((c.format, c.pax))
}

/// Sequential tar writer (Go `tar.Writer`, PAX/USTAR subset).
/// [`TarWriter::write_header`] begins a new file; the `io::Write` impl then
/// accepts that file's data.
pub(crate) struct TarWriter<W: io::Write> {
    w: W,
    /// Padding to write after the current file entry.
    pad: i64,
    /// Logical bytes remaining in the current file entry (Go
    /// `regFileWriter.nb`).
    remaining: i64,
}

impl<W: io::Write> TarWriter<W> {
    pub(crate) fn new(w: W) -> TarWriter<W> {
        TarWriter {
            w,
            pad: 0,
            remaining: 0,
        }
    }

    /// Finishes the current file's block padding; the current file must be
    /// fully written first (Go `Flush`).
    fn flush_padding(&mut self) -> Result<(), TwError> {
        if self.remaining > 0 {
            return Err(TwError::Missed(self.remaining as u64));
        }
        self.w
            .write_all(&[0u8; BLOCK_SIZE][..self.pad as usize])
            .map_err(TwError::Io)?;
        self.pad = 0;
        Ok(())
    }

    /// Writes `hdr` and prepares to accept the file's contents (Go
    /// `WriteHeader`).
    pub(crate) fn write_header(&mut self, hdr: &TarHeader) -> Result<(), TwError> {
        self.flush_padding()?;
        let mut hdr = hdr.clone();

        // Avoid the legacy TypeRegA flag; promote to TypeReg or TypeDir.
        if hdr.typeflag == TYPE_REG_A {
            hdr.typeflag = if hdr.name.ends_with(b"/") {
                TYPE_DIR
            } else {
                TYPE_REG
            };
        }

        // Round ModTime and drop AccessTime/ChangeTime unless the format is
        // explicitly chosen (avoids accidental PAX promotion).
        if hdr.format == F_UNKNOWN {
            hdr.mod_time = hdr.mod_time.map(PaxTime::round_to_second);
            hdr.access_time = None;
            hdr.change_time = None;
        }

        let (formats, pax_hdrs) = allowed_formats(&hdr)?;
        if formats & F_USTAR != 0 {
            self.write_ustar_header(&mut hdr)
        } else if formats & F_PAX != 0 {
            self.write_pax_header(&mut hdr, pax_hdrs)
        } else {
            // Only the GNU writer could encode this header; it is not ported
            // (unreachable while the format is pinned to PAX, as tarexport
            // pins it).
            Err(TwError::Header(vec![
                "GNU-format headers are not supported by this writer".into(),
            ]))
        }
    }

    fn write_ustar_header(&mut self, hdr: &mut TarHeader) -> Result<(), TwError> {
        // Check if we can use USTAR prefix/suffix splitting.
        let mut name_prefix: Vec<u8> = Vec::new();
        if let Some((prefix, suffix)) = split_ustar_path(&hdr.name) {
            name_prefix = prefix.to_vec();
            hdr.name = suffix.to_vec();
        }

        // Pack the main header.
        let mut f = Formatter::default();
        let mut blk = [0u8; BLOCK_SIZE];
        template_v7_plus(&mut blk, &mut f, hdr, false);
        f.format_string(fld_mut(&mut blk, USTAR_PREFIX), &name_prefix);
        set_format(&mut blk, F_USTAR);
        if f.field_too_long {
            // Should never happen since the header is validated.
            return Err(TwError::FieldTooLong);
        }
        self.write_raw_header(&blk, hdr.size, hdr.typeflag)
    }

    fn write_pax_header(
        &mut self,
        hdr: &mut TarHeader,
        pax_hdrs: PaxRecords,
    ) -> Result<(), TwError> {
        let real_name = hdr.name.clone();

        // Write PAX records to the output.
        let is_global = hdr.typeflag == TYPE_XGLOBAL_HEADER;
        if !pax_hdrs.is_empty() || is_global {
            // Write each record to a buffer, sorted by key (BTreeMap order
            // equals Go's sorted-string order: bytewise).
            let mut data: Vec<u8> = Vec::new();
            for (k, v) in &pax_hdrs {
                let rec = format_pax_record(k, v).ok_or(TwError::BadRecord)?;
                data.extend_from_slice(&rec);
            }

            // Write the extended header file.
            let (name, flag) = if is_global {
                let name = if real_name.is_empty() {
                    b"GlobalHead.0.0".to_vec()
                } else {
                    real_name.clone()
                };
                (name, TYPE_XGLOBAL_HEADER)
            } else {
                let (dir, file) = path_split(&real_name);
                (path_join(&[dir, b"PaxHeaders.0", file]), TYPE_XHEADER)
            };
            if data.len() > MAX_SPECIAL_FILE_SIZE {
                return Err(TwError::FieldTooLong);
            }
            self.write_raw_file(&name, &data, flag, F_PAX)?;
            if is_global {
                return Ok(()); // Global headers return here
            }
        }

        // Pack the main header (formatter errors are expected and ignored).
        let mut f = Formatter::default();
        let mut blk = [0u8; BLOCK_SIZE];
        template_v7_plus(&mut blk, &mut f, hdr, true);
        set_format(&mut blk, F_PAX);
        self.write_raw_header(&blk, hdr.size, hdr.typeflag)
    }

    /// Writes a minimal file with the given name and flag type, with default
    /// values for all other fields (Go `writeRawFile`).
    fn write_raw_file(
        &mut self,
        name: &[u8],
        data: &[u8],
        flag: u8,
        format: u8,
    ) -> Result<(), TwError> {
        let mut blk = [0u8; BLOCK_SIZE];

        // Best effort for the filename.
        let mut name = to_ascii(name);
        if name.len() > NAME_SIZE {
            name.truncate(NAME_SIZE);
        }
        while name.last() == Some(&b'/') {
            name.pop(); // TrimRight(name, "/")
        }

        let mut f = Formatter::default();
        blk[V7_TYPEFLAG] = flag;
        f.format_string(fld_mut(&mut blk, V7_NAME), &name);
        f.format_octal(fld_mut(&mut blk, V7_MODE), 0);
        f.format_octal(fld_mut(&mut blk, V7_UID), 0);
        f.format_octal(fld_mut(&mut blk, V7_GID), 0);
        f.format_octal(fld_mut(&mut blk, V7_SIZE), data.len() as i64); // Must be < 8GiB
        f.format_octal(fld_mut(&mut blk, V7_MTIME), 0);
        set_format(&mut blk, format);
        if f.field_too_long {
            // Only occurs if the size condition is violated.
            return Err(TwError::FieldTooLong);
        }

        // Write the header and data.
        self.write_raw_header(&blk, data.len() as i64, flag)?;
        io::Write::write_all(self, data).map_err(TwError::Io)
    }

    /// Writes the block and sets up the writer to accept a file of the given
    /// size (Go `writeRawHeader`; header-only flags force size 0).
    fn write_raw_header(
        &mut self,
        blk: &[u8; BLOCK_SIZE],
        size: i64,
        flag: u8,
    ) -> Result<(), TwError> {
        self.flush_padding()?;
        self.w.write_all(blk).map_err(TwError::Io)?;
        let size = if is_header_only(flag) { 0 } else { size };
        self.remaining = size;
        self.pad = block_padding(size);
        Ok(())
    }

    /// Flushes the padding and writes the two-zero-block trailer (Go
    /// `Close`; the write-after-close latch is not ported — this writer is
    /// single-shot).
    pub(crate) fn close(&mut self) -> Result<(), TwError> {
        self.flush_padding()?;
        for _ in 0..2 {
            self.w.write_all(&[0u8; BLOCK_SIZE]).map_err(TwError::Io)?;
        }
        Ok(())
    }
}

/// Fills the V7 and shared USTAR fields from `hdr` (Go `templateV7Plus`).
/// `ascii` selects the PAX string formatter (`toASCII` first).
fn template_v7_plus(blk: &mut [u8; BLOCK_SIZE], f: &mut Formatter, hdr: &TarHeader, ascii: bool) {
    let mod_time = hdr.mod_time.unwrap_or(PaxTime { sec: 0, nsec: 0 });

    fn fmt_str(f: &mut Formatter, b: &mut [u8], s: &[u8], ascii: bool) {
        if ascii {
            f.format_string(b, &to_ascii(s));
        } else {
            f.format_string(b, s);
        }
    }

    blk[V7_TYPEFLAG] = hdr.typeflag;
    fmt_str(f, fld_mut(blk, V7_NAME), &hdr.name, ascii);
    fmt_str(f, fld_mut(blk, V7_LINKNAME), &hdr.linkname, ascii);
    f.format_octal(fld_mut(blk, V7_MODE), hdr.mode);
    f.format_octal(fld_mut(blk, V7_UID), hdr.uid);
    f.format_octal(fld_mut(blk, V7_GID), hdr.gid);
    f.format_octal(fld_mut(blk, V7_SIZE), hdr.size);
    f.format_octal(fld_mut(blk, V7_MTIME), mod_time.sec);

    fmt_str(f, fld_mut(blk, USTAR_UNAME), &hdr.uname, ascii);
    fmt_str(f, fld_mut(blk, USTAR_GNAME), &hdr.gname, ascii);
    f.format_octal(fld_mut(blk, USTAR_DEVMAJOR), hdr.devmajor);
    f.format_octal(fld_mut(blk, USTAR_DEVMINOR), hdr.devminor);
}

/// Content writes for the current file entry (Go `Writer.Write` via
/// `regFileWriter`): writing past the header's size fails with
/// `archive/tar: write too long`.
impl<W: io::Write> io::Write for TarWriter<W> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        if self.remaining <= 0 {
            return Err(io::Error::other("archive/tar: write too long"));
        }
        let n = (self.remaining as u64).min(buf.len() as u64) as usize;
        let written = self.w.write(&buf[..n])?;
        self.remaining -= written as i64;
        Ok(written)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.w.flush()
    }
}

#[cfg(test)]
mod tests {
    /// Every ASCII byte, against what Go's `fmt.Sprintf("%q", ...)` printed
    /// for the same string.
    #[test]
    fn go_quote_is_exact_for_ascii() {
        let ascii: Vec<u8> = (0..0x80).collect();
        let want = r##""\x00\x01\x02\x03\x04\x05\x06\a\b\t\n\v\f\r\x0e\x0f\x10\x11\x12\x13\x14\x15\x16\x17\x18\x19\x1a\x1b\x1c\x1d\x1e\x1f !\"#$%&'()*+,-./0123456789:;<=>?@ABCDEFGHIJKLMNOPQRSTUVWXYZ[\\]^_`abcdefghijklmnopqrstuvwxyz{|}~\x7f""##;
        assert_eq!(super::GoQuote(&ascii).to_string(), want);
    }

    use std::collections::HashMap;

    use super::*;
    use crate::fstree::{encode_blob, encode_dir_leaf, encode_xattr_set};
    use crate::tarextract::TarReader;

    /// In-memory store mirroring the Go tests' packstore usage.
    #[derive(Default)]
    struct MemStore(HashMap<Key, Vec<u8>>);

    impl MemStore {
        fn put(&mut self, o: &fstree::Object) {
            self.0.insert(o.key, o.bytes.clone());
        }

        fn get(&self) -> impl FnMut(Key) -> Result<Vec<u8>, String> + '_ {
            |k| {
                self.0
                    .get(&k)
                    .cloned()
                    .ok_or_else(|| format!("object {k} not in store"))
            }
        }
    }

    /// Reads back all entries of a tar stream as (name, typeflag, content).
    fn read_all(mut buf: &[u8]) -> Vec<(Vec<u8>, u8, Vec<u8>)> {
        let mut tr = TarReader::new(&mut buf);
        let mut out = Vec::new();
        while let Some(h) = tr.next().expect("read tar") {
            let mut data = Vec::new();
            io::Read::read_to_end(&mut tr, &mut data).expect("read entry");
            out.push((h.name.clone(), h.typeflag, data));
        }
        out
    }

    // --- ports of Go tarexport_test.go ---

    #[test]
    fn write_regular_files_and_dir() {
        let mut store = MemStore::default();
        let ablob = encode_blob(b"alpha");
        let bblob = encode_blob(b"beta");
        store.put(&ablob);
        store.put(&bblob);

        let entries = [
            Entry {
                name: b"a".to_vec(),
                mode: 0o100644,
                mtime: 1,
                content_key: ablob.key.as_bytes().to_vec(),
                ..Default::default()
            },
            Entry {
                name: b"b".to_vec(),
                mode: 0o100644,
                mtime: 2,
                content_key: bblob.key.as_bytes().to_vec(),
                ..Default::default()
            },
        ];
        let leaf = encode_dir_leaf(&entries).unwrap();
        store.put(&leaf);

        let mut buf = Vec::new();
        write(&mut buf, leaf.key, store.get()).expect("Write");

        let got: HashMap<Vec<u8>, Vec<u8>> =
            read_all(&buf).into_iter().map(|(n, _, d)| (n, d)).collect();
        assert_eq!(got[&b"a".to_vec()], b"alpha");
        assert_eq!(got[&b"b".to_vec()], b"beta");
    }

    #[test]
    fn write_rejects_non_directory_root() {
        let mut store = MemStore::default();
        let blob = encode_blob(b"x");
        store.put(&blob);
        let mut buf = Vec::new();
        let err = write(&mut buf, blob.key, store.get()).unwrap_err();
        assert!(matches!(err, Error::NotDirectory { .. }));
        assert_eq!(
            err.to_string(),
            format!(
                "tarexport: root {} is not a directory object (type Blob)",
                blob.key
            )
        );
    }

    #[test]
    fn write_nested_dir() {
        let mut store = MemStore::default();
        let fblob = encode_blob(b"deep");
        store.put(&fblob);
        let child = encode_dir_leaf(&[Entry {
            name: b"f.txt".to_vec(),
            mode: 0o100644,
            mtime: 1,
            content_key: fblob.key.as_bytes().to_vec(),
            ..Default::default()
        }])
        .unwrap();
        store.put(&child);
        let root = encode_dir_leaf(&[Entry {
            name: b"d".to_vec(),
            mode: 0o040755, // S_IFDIR
            mtime: 2,
            content_key: child.key.as_bytes().to_vec(),
            ..Default::default()
        }])
        .unwrap();
        store.put(&root);

        let mut buf = Vec::new();
        write(&mut buf, root.key, store.get()).expect("Write");

        let entries = read_all(&buf);
        let types: HashMap<Vec<u8>, u8> = entries.iter().map(|(n, t, _)| (n.clone(), *t)).collect();
        let names: HashMap<Vec<u8>, Vec<u8>> =
            entries.into_iter().map(|(n, _, d)| (n, d)).collect();
        assert_eq!(types[&b"d/".to_vec()], TYPE_DIR);
        assert_eq!(names[&b"d/f.txt".to_vec()], b"deep");
    }

    #[test]
    fn write_rejects_unsafe_entry_name() {
        let mut store = MemStore::default();
        let blob = encode_blob(b"x");
        store.put(&blob);
        let leaf = encode_dir_leaf(&[Entry {
            name: b"..".to_vec(),
            mode: 0o100644,
            mtime: 1,
            content_key: blob.key.as_bytes().to_vec(),
            ..Default::default()
        }])
        .unwrap();
        store.put(&leaf);
        let mut buf = Vec::new();
        let err = write(&mut buf, leaf.key, store.get()).unwrap_err();
        assert!(matches!(err, Error::UnsafeName { .. }));
        assert_eq!(
            err.to_string(),
            "tarexport: refusing unsafe entry name \"..\""
        );
    }

    #[test]
    fn spilled_xattrs_are_exported() {
        let mut store = MemStore::default();
        let blob = encode_blob(b"content");
        store.put(&blob);
        let mut xm = BTreeMap::new();
        xm.insert(b"user.big".to_vec(), vec![7u8; 300]);
        let xset = encode_xattr_set(&xm);
        store.put(&xset);
        let leaf = encode_dir_leaf(&[Entry {
            name: b"f".to_vec(),
            mode: 0o100644,
            content_key: blob.key.as_bytes().to_vec(),
            xattrs_key: xset.key.as_bytes().to_vec(),
            ..Default::default()
        }])
        .unwrap();
        store.put(&leaf);

        let mut buf = Vec::new();
        write(&mut buf, leaf.key, store.get()).expect("Write");

        let mut r = &buf[..];
        let mut tr = TarReader::new(&mut r);
        let h = tr.next().expect("read").expect("entry");
        assert_eq!(h.name, b"f");
        assert_eq!(
            h.pax_records.get(b"SCHILY.xattr.user.big".as_slice()),
            Some(&vec![7u8; 300])
        );
    }

    // --- strconv/writer helper vectors (from Go archive/tar strconv_test.go
    // --- and path/path_test.go) ---

    #[test]
    fn format_pax_time_vectors() {
        for (sec, nsec, want) in [
            (1350244992i64, 0i64, "1350244992"),
            (1350244992, 300_000_000, "1350244992.3"),
            (1350244992, 23_960_100, "1350244992.0239601"),
            (1350244992, 23_960_108, "1350244992.023960108"),
            (1, 0, "1"),
            (2, 500_000_000, "2.5"),
            (0, 0, "0"),
            (-1, 0, "-1"),
            (-1, 999_999_999, "-0.000000001"),
            (-1, 1, "-0.999999999"),
            (-2, 500_000_000, "-1.5"),
        ] {
            let ts = PaxTime::unix(sec, nsec);
            assert_eq!(
                format_pax_time(ts),
                want.as_bytes(),
                "formatPAXTime({sec}, {nsec})"
            );
        }
    }

    #[test]
    fn format_pax_record_vectors() {
        for (k, v, want) in [
            ("k", "v", "6 k=v\n"),
            ("path", "/etc/hosts", "19 path=/etc/hosts\n"),
            (
                "mtime",
                "1350244992.023960108",
                "30 mtime=1350244992.023960108\n",
            ),
            ("dumb", "dumber", "15 dumb=dumber\n"),
            ("xhe", "adder", "13 xhe=adder\n"),
            // The one-round size adjustment: 9 would fit, but writing "10"
            // makes the record 11 bytes long.
            ("added", "5", "11 added=5\n"),
        ] {
            let got = format_pax_record(k.as_bytes(), v.as_bytes()).expect("valid record");
            assert_eq!(got, want.as_bytes(), "formatPAXRecord({k}, {v})");
        }
        // The length prefix always includes its own digits.
        for len in [1usize, 5, 88, 92, 995, 996] {
            let v = "a".repeat(len);
            let rec = format_pax_record(b"k", v.as_bytes()).unwrap();
            let n: usize =
                std::str::from_utf8(&rec[..rec.iter().position(|&b| b == b' ').unwrap()])
                    .unwrap()
                    .parse()
                    .unwrap();
            assert_eq!(n, rec.len(), "self-including length for value len {len}");
        }
        // Invalid records are rejected.
        assert!(format_pax_record(b"", b"v").is_none());
        assert!(format_pax_record(b"k=", b"v").is_none());
        assert!(format_pax_record(b"path", b"v\0v").is_none());
        // NULs are fine in non-basic values (SCHILY.xattr.*).
        assert!(format_pax_record(b"SCHILY.xattr.user.x", b"a\0b").is_some());
    }

    #[test]
    fn split_ustar_path_vectors() {
        let long = "a".repeat(100);
        let cases: Vec<(String, Option<(String, String)>)> = vec![
            (String::new(), None),
            ("abc".into(), None),
            ("用戶名".into(), None),
            (format!("{long}/"), None),
            (format!("{long}/a"), Some((long.clone(), "a".into()))),
            (format!("{long}/{long}"), Some((long.clone(), long.clone()))),
            (format!("{long}/{long}/b"), None), // suffix too long after truncation
            (format!("{long}/{long}x"), None),
        ];
        for (input, want) in cases {
            let got = split_ustar_path(input.as_bytes()).map(|(p, s)| {
                (
                    std::str::from_utf8(p).unwrap().to_string(),
                    std::str::from_utf8(s).unwrap().to_string(),
                )
            });
            assert_eq!(got, want, "splitUSTARPath({input:?})");
        }
    }

    #[test]
    fn path_clean_vectors() {
        for (input, want) in [
            ("", "."),
            ("abc", "abc"),
            ("abc/def", "abc/def"),
            (".", "."),
            ("..", ".."),
            ("abc/", "abc"),
            ("abc//def//ghi", "abc/def/ghi"),
            ("//abc", "/abc"),
            ("abc/./def", "abc/def"),
            ("/./abc/def", "/abc/def"),
            ("abc/..", "."),
            ("abc/def/..", "abc"),
            ("abc/def/../..", "."),
            ("abc/def/../../..", ".."),
            ("/abc/def/../../..", "/"),
            ("abc/./../def", "def"),
            ("abc/../../././../def", "../../def"),
        ] {
            assert_eq!(
                path_clean(input.as_bytes()),
                want.as_bytes(),
                "path.Clean({input:?})"
            );
        }
        assert_eq!(
            path_join(&[b"sub/", b"PaxHeaders.0", b"x"]),
            b"sub/PaxHeaders.0/x"
        );
        assert_eq!(path_join(&[b"", b"PaxHeaders.0", b"f"]), b"PaxHeaders.0/f");
        assert_eq!(
            path_join(&[b"sub/", b"PaxHeaders.0", b""]),
            b"sub/PaxHeaders.0"
        );
        assert_eq!(path_join(&[b"", b"a"]), b"a");
        assert_eq!(path_join(&[b"a", b"b"]), b"a/b");
    }
}
