//! Extracts a PAX tar (as produced by [`crate::tarexport`]) into a
//! directory, restoring permissions, ownership (when running as root),
//! extended attributes (best-effort), nanosecond mtimes, symlinks, fifos,
//! and device nodes. Directory permissions and mtimes are applied after all
//! members are written, so a read-only or past-dated directory does not
//! block writing its children — and creating children does not disturb a
//! restored directory's mtime.
//!
//! The tar reader is a hand-rolled port of the Go `archive/tar` reader
//! subset that covers what tarexport emits (PAX records incl. nanosecond
//! mtimes and `SCHILY.xattr.*`, USTAR prefixes, base-256 numerics, the
//! two-zero-block terminator). GNU long-name/long-link meta entries and
//! sparse files are not supported; see `port-notes/tar.md`.

use std::collections::BTreeMap;
use std::ffi::CString;
use std::fmt;
use std::fs;
use std::io;
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::os::unix::fs::DirBuilderExt;
use std::path::{Path, PathBuf};

use crate::tarexport::{
    BLOCK_SIZE, F_GNU, F_PAX, F_STAR, F_UNKNOWN, F_USTAR, F_V7, GNU_ATIME, GNU_CTIME, GoQuote,
    MAX_SPECIAL_FILE_SIZE, PaxRecords, PaxTime, STAR_ATIME, STAR_CTIME, STAR_PREFIX, STAR_TRAILER,
    TYPE_BLOCK, TYPE_CHAR, TYPE_DIR, TYPE_FIFO, TYPE_REG, TYPE_REG_A, TYPE_SYMLINK,
    TYPE_XGLOBAL_HEADER, TYPE_XHEADER, TarHeader, USTAR_DEVMAJOR, USTAR_DEVMINOR, USTAR_GNAME,
    USTAR_MAGIC, USTAR_PREFIX, USTAR_UNAME, USTAR_VERSION, V7_CHKSUM, V7_GID, V7_LINKNAME, V7_MODE,
    V7_MTIME, V7_NAME, V7_SIZE, V7_TYPEFLAG, V7_UID, block_padding, compute_checksum, fld,
    is_ascii_str, is_header_only, path_clean, path_join, valid_pax_record,
};

const XATTR_PREFIX: &[u8] = b"SCHILY.xattr.";

/// Errors from [`extract`], carrying the same diagnostic text as the Go
/// implementation.
#[derive(Debug)]
pub enum Error {
    /// An I/O failure reading the archive or writing files.
    Io(io::Error),
    /// `archive/tar: invalid tar header`
    Header,
    /// `archive/tar: header field too long` (an over-long PAX header).
    FieldTooLong,
    /// `refusing unsafe entry name "<name>"`
    UnsafeName {
        /// The refused member name.
        name: Vec<u8>,
    },
    /// `<target>: mkfifo: <err>`
    Mkfifo {
        /// The path being created.
        target: PathBuf,
        /// The OS error.
        source: io::Error,
    },
    /// `<target>: mknod: <err>`
    Mknod {
        /// The path being created.
        target: PathBuf,
        /// The OS error.
        source: io::Error,
    },
    /// `<target>: chmod: <err>`
    Chmod {
        /// The path being restored.
        target: PathBuf,
        /// The OS error.
        source: io::Error,
    },
    /// `<target>: chown: <err>`
    Chown {
        /// The path being restored.
        target: PathBuf,
        /// The OS error.
        source: io::Error,
    },
    /// `setting xattr "<name>" on <target>: <err>`
    Xattr {
        /// The extended attribute's name.
        name: Vec<u8>,
        /// The path being restored.
        target: PathBuf,
        /// The OS error.
        source: io::Error,
    },
    /// `<target>: set mtime: <err>`
    SetMtime {
        /// The path being restored.
        target: PathBuf,
        /// The OS error.
        source: io::Error,
    },
    /// `refusing to extract through non-directory <path>`
    ThroughNonDirectory {
        /// The existing non-directory component.
        path: PathBuf,
    },
    /// `<target>: unsupported tar type '<flag>'`
    Unsupported {
        /// The path the member would have created.
        target: PathBuf,
        /// The unsupported type flag.
        typeflag: u8,
    },
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Io(e) => e.fmt(f),
            Error::Header => write!(f, "archive/tar: invalid tar header"),
            Error::FieldTooLong => write!(f, "archive/tar: header field too long"),
            Error::UnsafeName { name } => {
                write!(f, "refusing unsafe entry name {}", GoQuote(name))
            }
            Error::Mkfifo { target, source } => {
                write!(f, "{}: mkfifo: {source}", target.display())
            }
            Error::Mknod { target, source } => write!(f, "{}: mknod: {source}", target.display()),
            Error::Chmod { target, source } => write!(f, "{}: chmod: {source}", target.display()),
            Error::Chown { target, source } => write!(f, "{}: chown: {source}", target.display()),
            Error::Xattr {
                name,
                target,
                source,
            } => write!(
                f,
                "setting xattr {} on {}: {source}",
                GoQuote(name),
                target.display()
            ),
            Error::SetMtime { target, source } => {
                write!(f, "{}: set mtime: {source}", target.display())
            }
            Error::ThroughNonDirectory { path } => {
                write!(
                    f,
                    "refusing to extract through non-directory {}",
                    path.display()
                )
            }
            Error::Unsupported { target, typeflag } => {
                // Go renders the flag with %q on a byte: a rune literal.
                write!(f, "{}: unsupported tar type ", target.display())?;
                let c = *typeflag;
                if (0x20..0x7f).contains(&c) {
                    write!(f, "'{}'", c as char)
                } else {
                    write!(f, "'\\x{c:02x}'")
                }
            }
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Error::Io(e)
            | Error::Mkfifo { source: e, .. }
            | Error::Mknod { source: e, .. }
            | Error::Chmod { source: e, .. }
            | Error::Chown { source: e, .. }
            | Error::Xattr { source: e, .. }
            | Error::SetMtime { source: e, .. } => Some(e),
            _ => None,
        }
    }
}

fn unexpected_eof() -> io::Error {
    io::Error::new(io::ErrorKind::UnexpectedEof, "unexpected EOF")
}

/// Reads a tar from `r` and materializes it under `dest_dir`, which is
/// created if necessary. `dest_dir` itself keeps default attributes (the
/// export root's own metadata is not part of the archive).
pub fn extract<R>(r: &mut R, dest_dir: &Path) -> Result<(), Error>
where
    R: io::Read + ?Sized,
{
    let mut db = fs::DirBuilder::new();
    db.recursive(true).mode(0o755);
    db.create(dest_dir).map_err(Error::Io)?;

    let mut tr = TarReader::new(r);
    let mut dirs: Vec<TarHeader> = Vec::new(); // directories, for deferred metadata
    while let Some(h) = tr.next()? {
        // Archive-wide metadata, not a member; `git archive` writes one.
        if h.typeflag == TYPE_XGLOBAL_HEADER {
            continue;
        }
        let target = safe_join(dest_dir, &h.name)?;
        // safe_join is lexical. An earlier symlink member could still lead
        // directory creation or the open out of dest_dir.
        reject_symlink_components(dest_dir, &target)?;
        match h.typeflag {
            TYPE_DIR => {
                let mut db = fs::DirBuilder::new();
                db.recursive(true).mode(0o700);
                db.create(&target).map_err(Error::Io)?;
                dirs.push(h);
            }
            TYPE_REG => {
                write_regular(&target, &mut tr)?;
                apply_meta(&target, &h, false)?;
            }
            TYPE_SYMLINK => {
                std::os::unix::fs::symlink(
                    Path::new(std::ffi::OsStr::from_bytes(&h.linkname)),
                    &target,
                )
                .map_err(Error::Io)?;
                apply_meta(&target, &h, true)?;
            }
            TYPE_FIFO => {
                mkfifo(&target, (h.mode & 0o7777) as u32).map_err(|source| Error::Mkfifo {
                    target: target.clone(),
                    source,
                })?;
                apply_meta(&target, &h, false)?;
            }
            TYPE_CHAR | TYPE_BLOCK => {
                let mut mode = (h.mode & 0o7777) as u32;
                if h.typeflag == TYPE_CHAR {
                    mode |= S_IFCHR;
                } else {
                    mode |= S_IFBLK;
                }
                let dev = mkdev(h.devmajor as u32, h.devminor as u32);
                if let Err(source) = mknod(&target, mode, dev) {
                    if is_privilege_error(&source) {
                        eprintln!(
                            "amber-store: skipping device node {}: {source}",
                            target.display()
                        );
                        continue;
                    }
                    return Err(Error::Mknod { target, source });
                }
                apply_meta(&target, &h, false)?;
            }
            flag => {
                return Err(Error::Unsupported {
                    target,
                    typeflag: flag,
                });
            }
        }
    }
    // Apply directory metadata last so child writes do not disturb it and a
    // read-only mode does not block them.
    for h in &dirs {
        let target = safe_join(dest_dir, &h.name)?;
        apply_meta(&target, h, false)?;
    }
    Ok(())
}

/// Fails if any existing component of `path` below `dest` is not a real
/// directory (Go: `rejectSymlinkComponents`).
fn reject_symlink_components(dest: &Path, path: &Path) -> Result<(), Error> {
    let mut p = path;
    while p != dest && p.starts_with(dest) {
        match fs::symlink_metadata(p) {
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => return Err(Error::Io(e)),
            Ok(fi) if !fi.is_dir() => {
                return Err(Error::ThroughNonDirectory {
                    path: p.to_path_buf(),
                });
            }
            Ok(_) => {}
        }
        let Some(parent) = p.parent() else { break };
        p = parent;
    }
    Ok(())
}

/// Joins `name` under `dest`, rejecting any name that escapes `dest`: names
/// containing `..` components (in any position) are refused, and the name is
/// re-rooted before joining so absolute names cannot escape either.
fn safe_join(dest: &Path, name: &[u8]) -> Result<PathBuf, Error> {
    for part in name.split(|&b| b == b'/') {
        if part == b".." {
            return Err(Error::UnsafeName {
                name: name.to_vec(),
            });
        }
    }
    let clean = path_clean(&[b"/", name].concat());
    let dest_bytes = dest.as_os_str().as_bytes();
    let target = path_join(&[dest_bytes, &clean]);
    if target != dest_bytes && !target.starts_with(&[dest_bytes, b"/"].concat()) {
        return Err(Error::UnsafeName {
            name: name.to_vec(),
        });
    }
    Ok(PathBuf::from(std::ffi::OsString::from_vec(target)))
}

fn write_regular<R: io::Read>(target: &Path, r: &mut R) -> Result<(), Error> {
    use std::os::unix::fs::OpenOptionsExt;
    let mut f = fs::OpenOptions::new()
        .write(true)
        .create_new(true) // O_WRONLY|O_CREATE|O_EXCL
        .mode(0o600)
        .open(target)
        .map_err(Error::Io)?;
    io::copy(r, &mut f).map_err(Error::Io)?;
    // Go checks f.Close(); Rust's drop closes without surfacing errors —
    // acceptable for O_WRONLY regular files (see port-notes/tar.md).
    Ok(())
}

/// Restores permissions, ownership, xattrs, and mtime for `target`.
/// Permissions and mtime are faithful; ownership only when running as root;
/// xattrs best-effort. chown runs before chmod because it clears
/// setuid/setgid. mtime is set last. Symlink targets are not chmod'd and
/// their xattrs are skipped.
fn apply_meta(target: &Path, h: &TarHeader, is_symlink: bool) -> Result<(), Error> {
    // SAFETY: geteuid takes no arguments and cannot fail.
    if unsafe { libc::geteuid() } == 0 {
        lchown(target, h.uid as libc::uid_t, h.gid as libc::gid_t).map_err(|source| {
            Error::Chown {
                target: target.to_path_buf(),
                source,
            }
        })?;
    }
    if !is_symlink {
        chmod(target, (h.mode & 0o7777) as u32).map_err(|source| Error::Chmod {
            target: target.to_path_buf(),
            source,
        })?;
        for (k, v) in &h.pax_records {
            let Some(name) = k.strip_prefix(XATTR_PREFIX) else {
                continue;
            };
            if let Err(source) = lsetxattr(target, name, v) {
                if is_privilege_error(&source) || source.raw_os_error() == Some(libc::ENOTSUP) {
                    eprintln!(
                        "amber-store: skipping xattr {} on {}: {source}",
                        GoQuote(name),
                        target.display()
                    );
                    continue;
                }
                return Err(Error::Xattr {
                    name: name.to_vec(),
                    target: target.to_path_buf(),
                    source,
                });
            }
        }
    }
    let ns = h
        .mod_time
        .unwrap_or(PaxTime { sec: 0, nsec: 0 })
        .unix_nanos();
    set_mtime(target, ns, is_symlink).map_err(|source| Error::SetMtime {
        target: target.to_path_buf(),
        source,
    })
}

fn set_mtime(target: &Path, ns: i64, is_symlink: bool) -> io::Result<()> {
    // Go unix.NsecToTimespec.
    let mut sec = ns / 1_000_000_000;
    let mut nsec = ns % 1_000_000_000;
    if nsec < 0 {
        nsec += 1_000_000_000;
        sec -= 1;
    }
    let ts = libc::timespec {
        tv_sec: sec as libc::time_t,
        tv_nsec: nsec as _,
    };
    let times = [ts, ts];
    let flags = if is_symlink {
        libc::AT_SYMLINK_NOFOLLOW
    } else {
        0
    };
    let c = cpath(target)?;
    // SAFETY: c is a valid NUL-terminated path and times points at two
    // initialized timespec values, per the utimensat contract.
    let rc = unsafe { libc::utimensat(libc::AT_FDCWD, c.as_ptr(), times.as_ptr(), flags) };
    if rc == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

fn is_privilege_error(err: &io::Error) -> bool {
    matches!(
        err.raw_os_error(),
        Some(libc::EPERM) | Some(libc::EACCES) | Some(libc::EOPNOTSUPP)
    )
}

// --- thin libc wrappers (Go golang.org/x/sys/unix equivalents) ---

const S_IFCHR: u32 = 0o020000;
const S_IFBLK: u32 = 0o060000;

fn cpath(p: &Path) -> io::Result<CString> {
    CString::new(p.as_os_str().as_bytes())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "path contains NUL"))
}

fn chmod(p: &Path, mode: u32) -> io::Result<()> {
    let c = cpath(p)?;
    // SAFETY: c is a valid NUL-terminated path.
    let rc = unsafe { libc::chmod(c.as_ptr(), mode as libc::mode_t) };
    if rc == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

fn lchown(p: &Path, uid: libc::uid_t, gid: libc::gid_t) -> io::Result<()> {
    let c = cpath(p)?;
    // SAFETY: c is a valid NUL-terminated path.
    let rc = unsafe { libc::lchown(c.as_ptr(), uid, gid) };
    if rc == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

fn mkfifo(p: &Path, mode: u32) -> io::Result<()> {
    let c = cpath(p)?;
    // SAFETY: c is a valid NUL-terminated path.
    let rc = unsafe { libc::mkfifo(c.as_ptr(), mode as libc::mode_t) };
    if rc == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

fn mknod(p: &Path, mode: u32, dev: u64) -> io::Result<()> {
    let c = cpath(p)?;
    // SAFETY: c is a valid NUL-terminated path.
    let rc = unsafe { libc::mknod(c.as_ptr(), mode as libc::mode_t, dev as libc::dev_t) };
    if rc == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

/// Go `unix.Mkdev` per-OS (the kernel's dev_t packing).
fn mkdev(major: u32, minor: u32) -> u64 {
    #[cfg(target_os = "macos")]
    {
        (u64::from(major) << 24) | u64::from(minor)
    }
    #[cfg(not(target_os = "macos"))]
    {
        // Linux (glibc) packing; other unixes fall back to it too.
        let (major, minor) = (u64::from(major), u64::from(minor));
        ((major & 0xfff) << 8)
            | (minor & 0xff)
            | ((major & !0xfffu64) << 32)
            | ((minor & 0xfffff00) << 12)
    }
}

/// Go `unix.Lsetxattr`: set without following symlinks.
fn lsetxattr(p: &Path, name: &[u8], value: &[u8]) -> io::Result<()> {
    let c = cpath(p)?;
    let cname = CString::new(name)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "xattr name contains NUL"))?;
    #[cfg(target_os = "macos")]
    // SAFETY: c/cname are valid NUL-terminated strings and value points at
    // value.len() initialized bytes, per the setxattr contract.
    let rc = unsafe {
        libc::setxattr(
            c.as_ptr(),
            cname.as_ptr(),
            value.as_ptr().cast(),
            value.len(),
            0,
            libc::XATTR_NOFOLLOW,
        )
    };
    #[cfg(not(target_os = "macos"))]
    // SAFETY: as above, per the lsetxattr contract.
    let rc = unsafe {
        libc::lsetxattr(
            c.as_ptr(),
            cname.as_ptr(),
            value.as_ptr().cast(),
            value.len(),
            0,
        )
    };
    if rc == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

// ===========================================================================
// Tar reader (Go archive/tar reader subset)
// ===========================================================================

/// Sequential tar reader (Go `tar.Reader`): [`TarReader::next`] advances to
/// the next file, then the `io::Read` impl serves that file's data.
pub(crate) struct TarReader<'a, R: io::Read + ?Sized> {
    r: &'a mut R,
    /// Padding (ignored) after the current file entry.
    pad: i64,
    /// Data bytes remaining in the current file entry.
    remaining: i64,
    blk: [u8; BLOCK_SIZE],
}

impl<'a, R: io::Read + ?Sized> TarReader<'a, R> {
    pub(crate) fn new(r: &'a mut R) -> TarReader<'a, R> {
        TarReader {
            r,
            pad: 0,
            remaining: 0,
            blk: [0u8; BLOCK_SIZE],
        }
    }

    /// Advances to the next entry, or `None` at the end of the archive (Go
    /// `Reader.Next`, minus GNU long-name/long-link and sparse handling).
    pub(crate) fn next(&mut self) -> Result<Option<TarHeader>, Error> {
        let mut pax_hdrs: Option<PaxRecords> = None;
        loop {
            // Discard the remainder of the file and any padding. A clean EOF
            // inside the padding ends the archive (Go's tryReadFull quirk).
            self.discard_current()?;
            if self.skip_padding()? {
                return Ok(None);
            }

            let Some(mut hdr) = self.read_header()? else {
                return Ok(None);
            };
            self.handle_regular_file(&hdr)?;

            match hdr.typeflag {
                TYPE_XHEADER | TYPE_XGLOBAL_HEADER => {
                    let recs = parse_pax(&self.read_special_file()?)?;
                    if hdr.typeflag == TYPE_XGLOBAL_HEADER {
                        merge_pax(&mut hdr, recs)?;
                        return Ok(Some(TarHeader {
                            name: hdr.name,
                            typeflag: hdr.typeflag,
                            pax_records: hdr.pax_records,
                            ..TarHeader::default()
                        }));
                    }
                    pax_hdrs = Some(recs); // A meta header affecting the next header.
                }
                _ => {
                    if let Some(recs) = pax_hdrs.take() {
                        merge_pax(&mut hdr, recs)?;
                    }
                    if hdr.typeflag == TYPE_REG_A {
                        // Legacy archives use a trailing slash for directories.
                        hdr.typeflag = if hdr.name.ends_with(b"/") {
                            TYPE_DIR
                        } else {
                            TYPE_REG
                        };
                    }
                    // The extended headers may have updated the size.
                    self.handle_regular_file(&hdr)?;
                    return Ok(Some(hdr));
                }
            }
        }
    }

    /// Sets up the entry reader and padding for the data section (Go
    /// `handleRegularFile`).
    fn handle_regular_file(&mut self, hdr: &TarHeader) -> Result<(), Error> {
        let mut nb = hdr.size;
        if is_header_only(hdr.typeflag) {
            nb = 0;
        }
        if nb < 0 {
            return Err(Error::Header);
        }
        self.pad = block_padding(nb);
        self.remaining = nb;
        Ok(())
    }

    /// Reads and parses one header block (Go `readHeader`); `None` on the
    /// two-zero-block terminator or a clean EOF at a block boundary.
    fn read_header(&mut self) -> Result<Option<TarHeader>, Error> {
        if !self.read_block()? {
            return Ok(None); // EOF is okay here; exactly 0 bytes read
        }
        if self.blk.iter().all(|&b| b == 0) {
            if !self.read_block()? {
                return Ok(None); // EOF is okay here; exactly 1 block of zeros read
            }
            if self.blk.iter().all(|&b| b == 0) {
                return Ok(None); // normal EOF; exactly 2 blocks of zeros read
            }
            return Err(Error::Header); // Zero block and then non-zero block
        }

        // Verify the header matches a known format.
        let format = self.get_format();
        if format == F_UNKNOWN {
            return Err(Error::Header);
        }

        let mut p = Parser::default();
        let blk = &self.blk;
        let mut hdr = TarHeader {
            typeflag: blk[V7_TYPEFLAG],
            name: parse_string(fld(blk, V7_NAME)),
            linkname: parse_string(fld(blk, V7_LINKNAME)),
            ..TarHeader::default()
        };
        hdr.size = p.parse_numeric(fld(blk, V7_SIZE));
        hdr.mode = p.parse_numeric(fld(blk, V7_MODE));
        hdr.uid = p.parse_numeric(fld(blk, V7_UID));
        hdr.gid = p.parse_numeric(fld(blk, V7_GID));
        hdr.mod_time = Some(PaxTime::unix(p.parse_numeric(fld(blk, V7_MTIME)), 0));

        // Unpack format specific fields.
        if format > F_V7 {
            hdr.uname = parse_string(fld(blk, USTAR_UNAME));
            hdr.gname = parse_string(fld(blk, USTAR_GNAME));
            hdr.devmajor = p.parse_numeric(fld(blk, USTAR_DEVMAJOR));
            hdr.devminor = p.parse_numeric(fld(blk, USTAR_DEVMINOR));

            let mut prefix: Vec<u8> = Vec::new();
            if format & (F_USTAR | F_PAX) != 0 {
                prefix = parse_string(fld(blk, USTAR_PREFIX));
                // (Go additionally demotes the format guess when the block
                // has non-ASCII bytes or unterminated numeric fields; the
                // guess is unused by extract and is not carried.)
            } else if format & F_STAR != 0 {
                prefix = parse_string(fld(blk, STAR_PREFIX));
                hdr.access_time = Some(PaxTime::unix(p.parse_numeric(fld(blk, STAR_ATIME)), 0));
                hdr.change_time = Some(PaxTime::unix(p.parse_numeric(fld(blk, STAR_CTIME)), 0));
            } else if format & F_GNU != 0 {
                let mut p2 = Parser::default();
                if blk[GNU_ATIME.0] != 0 {
                    hdr.access_time = Some(PaxTime::unix(p2.parse_numeric(fld(blk, GNU_ATIME)), 0));
                }
                if blk[GNU_CTIME.0] != 0 {
                    hdr.change_time = Some(PaxTime::unix(p2.parse_numeric(fld(blk, GNU_CTIME)), 0));
                }
                if p2.err {
                    // Skeptical parsing for pre-Go1.8 writer output that
                    // treated this region as a USTAR prefix field.
                    hdr.access_time = None;
                    hdr.change_time = None;
                    let s = parse_string(fld(blk, USTAR_PREFIX));
                    if is_ascii_str(&s) || s.is_empty() {
                        prefix = s;
                    }
                }
            }
            if !prefix.is_empty() {
                hdr.name = [prefix.as_slice(), b"/", &hdr.name].concat();
            }
        }
        if p.err {
            return Err(Error::Header);
        }
        Ok(Some(hdr))
    }

    /// Go `block.getFormat`: checksum verification plus magic sniffing.
    fn get_format(&self) -> u8 {
        let mut p = Parser::default();
        let value = p.parse_octal(fld(&self.blk, V7_CHKSUM));
        let (chksum1, chksum2) = compute_checksum(&self.blk);
        if p.err || (value != chksum1 && value != chksum2) {
            return F_UNKNOWN;
        }
        let magic = fld(&self.blk, USTAR_MAGIC);
        let version = fld(&self.blk, USTAR_VERSION);
        let trailer = fld(&self.blk, STAR_TRAILER);
        if magic == b"ustar\0" && trailer == b"tar\0" {
            F_STAR
        } else if magic == b"ustar\0" {
            F_USTAR | F_PAX
        } else if magic == b"ustar " && version == b" \0" {
            F_GNU
        } else {
            F_V7
        }
    }

    /// Reads one 512-byte block; `false` on a clean EOF before any byte.
    fn read_block(&mut self) -> Result<bool, Error> {
        let mut n = 0;
        while n < BLOCK_SIZE {
            let r = self.r.read(&mut self.blk[n..]).map_err(Error::Io)?;
            if r == 0 {
                if n == 0 {
                    return Ok(false);
                }
                return Err(Error::Io(unexpected_eof()));
            }
            n += r;
        }
        Ok(true)
    }

    /// Discards the rest of the current entry's data (Go `discard`; a short
    /// stream is an error).
    fn discard_current(&mut self) -> Result<(), Error> {
        let mut buf = [0u8; 4096];
        while self.remaining > 0 {
            let n = (self.remaining as u64).min(buf.len() as u64) as usize;
            let r = self.r.read(&mut buf[..n]).map_err(Error::Io)?;
            if r == 0 {
                return Err(Error::Io(unexpected_eof()));
            }
            self.remaining -= r as i64;
        }
        Ok(())
    }

    /// Skips the block padding after an entry; reports `true` when the
    /// stream cleanly ended inside it (which Go treats as end-of-archive).
    fn skip_padding(&mut self) -> Result<bool, Error> {
        let mut left = self.pad;
        self.pad = 0;
        let mut buf = [0u8; BLOCK_SIZE];
        while left > 0 {
            let r = self.r.read(&mut buf[..left as usize]).map_err(Error::Io)?;
            if r == 0 {
                return Ok(true);
            }
            left -= r as i64;
        }
        Ok(false)
    }

    /// Go `readSpecialFile`: the whole current entry, capped at 1 MiB.
    fn read_special_file(&mut self) -> Result<Vec<u8>, Error> {
        let mut buf = Vec::new();
        let mut chunk = [0u8; 4096];
        loop {
            let n = io::Read::read(self, &mut chunk).map_err(Error::Io)?;
            if n == 0 {
                return Ok(buf);
            }
            buf.extend_from_slice(&chunk[..n]);
            if buf.len() > MAX_SPECIAL_FILE_SIZE {
                return Err(Error::FieldTooLong);
            }
        }
    }
}

/// Data reads for the current entry (Go `Reader.Read` via `regFileReader`):
/// `Ok(0)` at the end of the entry, `UnexpectedEof` on a truncated stream.
impl<R: io::Read + ?Sized> io::Read for TarReader<'_, R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if buf.is_empty() || self.remaining <= 0 {
            return Ok(0);
        }
        let n = (self.remaining as u64).min(buf.len() as u64) as usize;
        let r = self.r.read(&mut buf[..n])?;
        if r == 0 {
            return Err(unexpected_eof());
        }
        self.remaining -= r as i64;
        Ok(r)
    }
}

/// Go `parseString`: a NUL-terminated C-style string (the whole field when
/// no NUL is present).
fn parse_string(b: &[u8]) -> Vec<u8> {
    match b.iter().position(|&c| c == 0) {
        Some(i) => b[..i].to_vec(),
        None => b.to_vec(),
    }
}

/// Numeric-field parser with Go's sticky `ErrHeader`.
#[derive(Default)]
struct Parser {
    err: bool,
}

impl Parser {
    /// Go `parseOctal`: leading/trailing NULs and spaces are trimmed; empty
    /// means zero.
    fn parse_octal(&mut self, b: &[u8]) -> i64 {
        let start = b
            .iter()
            .position(|&c| c != b' ' && c != 0)
            .unwrap_or(b.len());
        let end = b
            .iter()
            .rposition(|&c| c != b' ' && c != 0)
            .map_or(start, |i| i + 1);
        let b = &b[start..end];
        if b.is_empty() {
            return 0;
        }
        // ParseUint(parseString(b), 8, 64).
        let s = parse_string(b);
        let mut x: u64 = 0;
        for &c in &s {
            if !(b'0'..=b'7').contains(&c) {
                self.err = true;
                return 0;
            }
            let Some(shifted) = x.checked_mul(8) else {
                self.err = true;
                return 0;
            };
            x = shifted + u64::from(c - b'0');
        }
        if s.is_empty() {
            self.err = true; // ParseUint("") fails (all-NUL after a NUL cut)
            return 0;
        }
        x as i64
    }

    /// Go `parseNumeric`: base-256 (GNU binary) when the top bit of the
    /// first byte is set, octal otherwise.
    fn parse_numeric(&mut self, b: &[u8]) -> i64 {
        if !b.is_empty() && b[0] & 0x80 != 0 {
            // Inversion mask handles negative numbers via -a-1 == ^a.
            let inv: u8 = if b[0] & 0x40 != 0 { 0xff } else { 0x00 };
            let mut x: u64 = 0;
            for (i, &c0) in b.iter().enumerate() {
                let mut c = c0 ^ inv;
                if i == 0 {
                    c &= 0x7f; // Ignore signal bit in first byte
                }
                if (x >> 56) > 0 {
                    self.err = true; // Integer overflow
                    return 0;
                }
                x = x << 8 | u64::from(c);
            }
            if (x >> 63) > 0 {
                self.err = true; // Integer overflow
                return 0;
            }
            if inv == 0xff {
                return !(x as i64);
            }
            return x as i64;
        }
        self.parse_octal(b)
    }
}

/// Go `parsePAX`: splits the extended-header data into records. (The GNU
/// sparse 0.0 map transformation is not ported — sparse archives are out of
/// scope.)
fn parse_pax(buf: &[u8]) -> Result<PaxRecords, Error> {
    let mut sbuf = buf;
    let mut pax_hdrs = BTreeMap::new();
    while !sbuf.is_empty() {
        let (k, v, rest) = parse_pax_record(sbuf)?;
        pax_hdrs.insert(k.to_vec(), v.to_vec());
        sbuf = rest;
    }
    Ok(pax_hdrs)
}

/// A parsed PAX record: key, value, and the remaining unparsed input.
type PaxRecordParts<'a> = (&'a [u8], &'a [u8], &'a [u8]);

/// Go `parsePAXRecord`: `"%d %s=%s\n"` with the self-including length.
fn parse_pax_record(s: &[u8]) -> Result<PaxRecordParts<'_>, Error> {
    // The size field ends at the first space.
    let sp = s.iter().position(|&b| b == b' ').ok_or(Error::Header)?;
    let (n_str, rest) = (&s[..sp], &s[sp + 1..]);

    // Parse the first token as a decimal integer.
    let n: i64 = std::str::from_utf8(n_str)
        .ok()
        .and_then(|t| t.parse().ok())
        .ok_or(Error::Header)?;
    if n < 5 || n > s.len() as i64 {
        return Err(Error::Header);
    }
    let n = n - (n_str.len() as i64 + 1); // convert from index in s to index in rest
    if n <= 0 {
        return Err(Error::Header);
    }
    let n = n as usize;

    // Extract everything between the space and the final newline.
    let (rec, nl, rem) = (&rest[..n - 1], &rest[n - 1..n], &rest[n..]);
    if nl != b"\n" {
        return Err(Error::Header);
    }

    // The first equals separates the key from the value.
    let eq = rec.iter().position(|&b| b == b'=').ok_or(Error::Header)?;
    let (k, v) = (&rec[..eq], &rec[eq + 1..]);

    if !valid_pax_record(k, v) {
        return Err(Error::Header);
    }
    Ok((k, v, rem))
}

/// Go `parsePAXTime`: `%d[.%d]`, with sub-second digits truncated to
/// nanoseconds and negative timestamps supported.
fn parse_pax_time(s: &[u8]) -> Result<PaxTime, Error> {
    const MAX_NANO_SECOND_DIGITS: usize = 9;

    // Split string into seconds and sub-seconds parts.
    let (ss, sn) = match s.iter().position(|&b| b == b'.') {
        Some(i) => (&s[..i], &s[i + 1..]),
        None => (s, &s[..0]),
    };

    // Parse the seconds.
    let secs: i64 = std::str::from_utf8(ss)
        .ok()
        .and_then(|t| t.parse().ok())
        .ok_or(Error::Header)?;
    if sn.is_empty() {
        return Ok(PaxTime::unix(secs, 0)); // No sub-second values
    }

    // Parse the nanoseconds, right-padded with '0's.
    let mut nano_digits = [b'0'; MAX_NANO_SECOND_DIGITS];
    for (i, &c) in sn.iter().enumerate() {
        if !c.is_ascii_digit() {
            return Err(Error::Header);
        }
        if i < MAX_NANO_SECOND_DIGITS {
            nano_digits[i] = c;
        }
    }
    let nsecs: i64 = std::str::from_utf8(&nano_digits)
        .expect("digits")
        .parse()
        .expect("digits parse");
    if !ss.is_empty() && ss[0] == b'-' {
        Ok(PaxTime::unix(secs, -nsecs)) // Negative correction
    } else {
        Ok(PaxTime::unix(secs, nsecs))
    }
}

/// Go `mergePAX`: folds the extended-header records into the header fields;
/// empty values keep the original USTAR value.
fn merge_pax(hdr: &mut TarHeader, pax_hdrs: PaxRecords) -> Result<(), Error> {
    for (k, v) in &pax_hdrs {
        if v.is_empty() {
            continue; // Keep the original USTAR value
        }
        let parse_i64 = |v: &[u8]| -> Result<i64, Error> {
            std::str::from_utf8(v)
                .ok()
                .and_then(|t| t.parse().ok())
                .ok_or(Error::Header)
        };
        match k.as_slice() {
            b"path" => hdr.name = v.clone(),
            b"linkpath" => hdr.linkname = v.clone(),
            b"uname" => hdr.uname = v.clone(),
            b"gname" => hdr.gname = v.clone(),
            b"uid" => hdr.uid = parse_i64(v)?,
            b"gid" => hdr.gid = parse_i64(v)?,
            b"atime" => hdr.access_time = Some(parse_pax_time(v)?),
            b"mtime" => hdr.mod_time = Some(parse_pax_time(v)?),
            b"ctime" => hdr.change_time = Some(parse_pax_time(v)?),
            b"size" => hdr.size = parse_i64(v)?,
            _ => {} // SCHILY.xattr.* and friends stay in pax_records
        }
    }
    hdr.pax_records = pax_hdrs;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tarexport::{TarWriter, format_pax_time};
    use std::io::Write as _;
    use std::os::unix::fs::MetadataExt;

    fn pax_header(name: &[u8], typeflag: u8) -> TarHeader {
        TarHeader {
            typeflag,
            name: name.to_vec(),
            format: F_PAX,
            ..TarHeader::default()
        }
    }

    // --- ports of Go tarextract_test.go ---

    #[test]
    fn extract_files_dirs_and_deferred_dir_meta() {
        let mtime = PaxTime::unix(1_700_000_000, 222_000_000);

        let mut buf: Vec<u8> = Vec::new();
        let mut tw = TarWriter::new(&mut buf);
        // A read-only directory listed BEFORE its child file. If dir mode
        // were applied immediately, writing the child would fail; deferral
        // makes it work.
        let mut dir = pax_header(b"ro/", TYPE_DIR);
        dir.mode = 0o500;
        dir.mod_time = Some(mtime);
        tw.write_header(&dir).unwrap();
        let mut file = pax_header(b"ro/child.txt", TYPE_REG);
        file.mode = 0o644;
        file.size = 5;
        file.mod_time = Some(mtime);
        tw.write_header(&file).unwrap();
        tw.write_all(b"hello").unwrap();
        tw.close().unwrap();

        let tmp = tempfile::tempdir().unwrap();
        let dest = tmp.path().join("out");
        extract(&mut &buf[..], &dest).expect("Extract");

        let got = fs::read(dest.join("ro").join("child.txt")).expect("reading child");
        assert_eq!(got, b"hello");
        let di = fs::symlink_metadata(dest.join("ro")).unwrap();
        assert_eq!(di.mode() & 0o7777, 0o500, "dir perm applied after children");
        assert_eq!(di.mtime(), 1_700_000_000);
        assert_eq!(di.mtime_nsec(), 222_000_000);
        // Restore write permission so the tempdir cleanup can remove it.
        fs::set_permissions(
            dest.join("ro"),
            std::os::unix::fs::PermissionsExt::from_mode(0o700),
        )
        .unwrap();
    }

    #[test]
    fn extract_rejects_unsafe_name() {
        let mut buf: Vec<u8> = Vec::new();
        let mut tw = TarWriter::new(&mut buf);
        let mut h = pax_header(b"../escape", TYPE_REG);
        h.mode = 0o644;
        h.size = 1;
        tw.write_header(&h).unwrap();
        tw.write_all(b"x").unwrap();
        tw.close().unwrap();

        let tmp = tempfile::tempdir().unwrap();
        let dest = tmp.path().join("out");
        let err = extract(&mut &buf[..], &dest).unwrap_err();
        assert!(matches!(err, Error::UnsafeName { .. }));
        assert_eq!(err.to_string(), "refusing unsafe entry name \"../escape\"");
    }

    #[test]
    fn extract_skips_pax_global_header() {
        let mut buf: Vec<u8> = Vec::new();
        let mut tw = TarWriter::new(&mut buf);
        let mut g = pax_header(b"pax_global_header", TYPE_XGLOBAL_HEADER);
        g.pax_records
            .insert(b"comment".to_vec(), b"0e7b0ccc10ca".to_vec());
        tw.write_header(&g).unwrap();
        let mut dir = pax_header(b"source/", TYPE_DIR);
        dir.mode = 0o755;
        tw.write_header(&dir).unwrap();
        let mut file = pax_header(b"source/README", TYPE_REG);
        file.mode = 0o644;
        file.size = 2;
        tw.write_header(&file).unwrap();
        tw.write_all(b"hi").unwrap();
        tw.close().unwrap();

        let tmp = tempfile::tempdir().unwrap();
        let dest = tmp.path().join("out");
        extract(&mut &buf[..], &dest).expect("Extract");
        assert_eq!(fs::read(dest.join("source/README")).unwrap(), b"hi");
        assert!(!dest.join("pax_global_header").exists());
    }

    #[test]
    fn extract_rejects_unsupported_type() {
        let mut buf: Vec<u8> = Vec::new();
        let mut tw = TarWriter::new(&mut buf);
        let mut h = pax_header(b"h", TYPE_LINK_TEST);
        h.linkname = b"t".to_vec();
        tw.write_header(&h).unwrap();
        tw.close().unwrap();

        let tmp = tempfile::tempdir().unwrap();
        let dest = tmp.path().join("out");
        let err = extract(&mut &buf[..], &dest).unwrap_err();
        assert!(matches!(err, Error::Unsupported { typeflag: b'1', .. }));
        assert!(
            err.to_string().ends_with("unsupported tar type '1'"),
            "{err}"
        );
    }

    const TYPE_LINK_TEST: u8 = b'1'; // hard link: tarexport never emits it

    /// Go's `buildTar`: a PAX archive of the given members, regular files
    /// filled with `size` bytes of `x`.
    fn build_tar(entries: Vec<TarHeader>) -> Vec<u8> {
        let mut buf: Vec<u8> = Vec::new();
        let mut tw = TarWriter::new(&mut buf);
        for mut h in entries {
            h.format = F_PAX;
            h.mod_time = Some(PaxTime::unix(1_700_000_000, 0));
            let (typeflag, size) = (h.typeflag, h.size);
            tw.write_header(&h).unwrap();
            if typeflag == TYPE_REG {
                tw.write_all(&vec![b'x'; size as usize]).unwrap();
            }
        }
        tw.close().unwrap();
        buf
    }

    // A symlink member followed by entries beneath it must not let the
    // archive write through the link to a location outside dest_dir (Go:
    // TestExtract_RejectsWriteThroughSymlink).
    #[test]
    fn extract_rejects_write_through_symlink() {
        let outside = tempfile::tempdir().unwrap();
        let sym = |name: &[u8]| {
            let mut h = pax_header(name, TYPE_SYMLINK);
            h.linkname = outside.path().as_os_str().as_bytes().to_vec();
            h
        };
        let reg = |name: &[u8]| {
            let mut h = pax_header(name, TYPE_REG);
            h.mode = 0o644;
            h.size = 1;
            h
        };
        let dir = |name: &[u8]| {
            let mut h = pax_header(name, TYPE_DIR);
            h.mode = 0o755;
            h
        };
        let cases: Vec<(&str, Vec<TarHeader>)> = vec![
            ("file through link", vec![sym(b"a"), reg(b"a/pwned")]),
            (
                "dir through link",
                vec![sym(b"a"), dir(b"a/sub/"), reg(b"a/sub/pwned")],
            ),
            ("dir entry on top of link", vec![sym(b"a"), dir(b"a/")]),
        ];
        for (name, entries) in cases {
            let buf = build_tar(entries);
            let tmp = tempfile::tempdir().unwrap();
            let dest = tmp.path().join("out");
            assert!(
                extract(&mut &buf[..], &dest).is_err(),
                "{name}: expected error"
            );
            let leaked: Vec<_> = fs::read_dir(outside.path())
                .unwrap()
                .map(|e| e.unwrap().file_name())
                .collect();
            assert!(
                leaked.is_empty(),
                "{name}: wrote outside dest_dir: {leaked:?}"
            );
        }
    }

    // The regular case still works: a symlink member sitting next to, not
    // above, the entries that follow it (Go: TestExtract_SymlinkSibling).
    #[test]
    fn extract_symlink_sibling() {
        let mut d = pax_header(b"d/", TYPE_DIR);
        d.mode = 0o755;
        let mut link = pax_header(b"d/link", TYPE_SYMLINK);
        link.linkname = b"/etc".to_vec();
        let mut f = pax_header(b"d/f", TYPE_REG);
        f.mode = 0o644;
        f.size = 1;
        let buf = build_tar(vec![d, link, f]);

        let tmp = tempfile::tempdir().unwrap();
        let dest = tmp.path().join("out");
        extract(&mut &buf[..], &dest).expect("extract");
        fs::metadata(dest.join("d").join("f")).expect("d/f");
    }

    // chown(2) strips setuid/setgid, so ownership must be restored before
    // the mode. Only observable when extract actually chowns, i.e. as root
    // (Go: TestExtract_SetuidSurvivesChown).
    #[test]
    fn extract_setuid_survives_chown() {
        // SAFETY: geteuid takes no arguments and cannot fail.
        if unsafe { libc::geteuid() } != 0 {
            return; // needs root to exercise the chown path
        }
        let mut h = pax_header(b"suid", TYPE_REG);
        h.mode = 0o4755;
        h.size = 1;
        h.uid = 1;
        h.gid = 1;
        let buf = build_tar(vec![h]);
        let tmp = tempfile::tempdir().unwrap();
        let dest = tmp.path().join("out");
        extract(&mut &buf[..], &dest).expect("extract");
        let fi = fs::symlink_metadata(dest.join("suid")).unwrap();
        assert_ne!(fi.mode() & 0o4000, 0, "setuid bit lost: {:o}", fi.mode());
    }

    // --- safeJoin ---

    #[test]
    fn safe_join_cases() {
        let dest = Path::new("/tmp/dest");
        let ok = |name: &[u8]| safe_join(dest, name).expect("safe");
        assert_eq!(ok(b"a/b"), PathBuf::from("/tmp/dest/a/b"));
        assert_eq!(ok(b"sub/"), PathBuf::from("/tmp/dest/sub"));
        // Absolute names are re-rooted under dest, not rejected.
        assert_eq!(ok(b"/abs"), PathBuf::from("/tmp/dest/abs"));
        // ".." anywhere is refused, even when it would not escape.
        for name in [&b"../x"[..], b"a/../b", b"a/b/..", b".."] {
            let err = safe_join(dest, name).unwrap_err();
            assert!(matches!(err, Error::UnsafeName { .. }), "{name:?}");
        }
    }

    // --- reader primitives (vectors from Go strconv_test.go) ---

    #[test]
    fn parse_pax_time_vectors() {
        for (input, want) in [
            ("1350244992.023960108", Some((1350244992i64, 23960108u32))),
            ("1350244992.02396010", Some((1350244992, 23960100))),
            ("1350244992.0239601089", Some((1350244992, 23960108))),
            ("1350244992.3", Some((1350244992, 300000000))),
            ("1350244992", Some((1350244992, 0))),
            ("-1.000000001", Some((-2, 999999999))),
            ("-1.000001", Some((-2, 999999000))),
            ("-13502449943", Some((-13502449943, 0))),
            ("9223372036854775807", Some((9223372036854775807, 0))),
            ("1.", Some((1, 0))),
            ("0.0", Some((0, 0))),
            (".5", None),
            ("", None),
            ("1.2.3", None),
            ("α", None),
            ("1.α", None),
        ] {
            let got = parse_pax_time(input.as_bytes())
                .ok()
                .map(|t| (t.sec, t.nsec));
            assert_eq!(got, want, "parsePAXTime({input:?})");
        }
        // Round-trips with the writer's formatter.
        for (sec, nsec) in [(1600000000i64, 500i64), (-1, 999999999), (0, 1)] {
            let ts = PaxTime::unix(sec, nsec);
            let s = format_pax_time(ts);
            assert_eq!(parse_pax_time(&s).unwrap(), ts, "round-trip {sec}.{nsec}");
        }
    }

    #[test]
    fn parse_pax_record_vectors() {
        // (input, want (k, v, rest) or None)
        let cases: &[(&[u8], Option<PaxRecordParts<'_>>)] = &[
            (b"6 k=v\n", Some((b"k", b"v", b""))),
            (b"6 k=v\nabc", Some((b"k", b"v", b"abc"))),
            (b"19 path=/etc/hosts\n", Some((b"path", b"/etc/hosts", b""))),
            (
                b"30 mtime=1350244992.023960108\n",
                Some((b"mtime", b"1350244992.023960108", b"")),
            ),
            (b"8 k~1=v\n", Some((b"k~1", b"v", b""))),
            (b"6_k=v\n", None), // no space after the length
            (b"6 k=v ", None),  // no trailing newline
            (b"0 k=v\n", None), // length too small
            (b"1 k=v\n", None),
            (b"-6 k=v\n", None),
            (b"999 k=v\n", None),                  // length past the buffer
            (b"6 kv=\n", Some((b"kv", b"", b""))), // empty value is fine
            (b"5 kv\n", None),                     // no '='
            (b"5 =v\n", None),                     // empty key
            (b"16 longkeys=hi\nxx", None),         // newline not at claimed length
        ];
        for (input, want) in cases {
            let got = parse_pax_record(input).ok();
            assert_eq!(
                got,
                *want,
                "parsePAXRecord({:?})",
                String::from_utf8_lossy(input)
            );
        }
    }

    #[test]
    fn parse_numeric_vectors() {
        // From Go strconv_test.go TestParseNumeric (subset).
        let octal = |s: &[u8]| {
            let mut p = Parser::default();
            let v = p.parse_numeric(s);
            (v, p.err)
        };
        assert_eq!(octal(b"0000000\x00"), (0, false));
        assert_eq!(octal(b" \x0000000\x00"), (0, false));
        assert_eq!(octal(b"00000000227\x00"), (0o227, false));
        assert_eq!(octal(b"032033\x00 "), (0o32033, false));
        assert_eq!(octal(b"320330\x00 "), (0o320330, false));
        assert_eq!(octal(b"0000660\x00 "), (0o660, false));
        assert_eq!(octal(b"\x00 0000660\x00 "), (0o660, false));
        assert!(octal(b"0123456789abcdef").1);
        assert!(octal(b"0123456789\x00abcdef").1);
        assert_eq!(octal(b"\x80\x00\x00\x00\x00\x00\x00\x01"), (1, false));
        assert_eq!(
            octal(b"\x80\x00\x00\x00\x07\x76\xa2\x22\xeb\x8a\x72\x61"),
            (537795476381659745, false)
        );
        assert_eq!(octal(b"\xff\xff\xff\xff\xff\xff\xff\xff"), (-1, false));
        assert_eq!(
            octal(b"\xff\xff\xff\xff\xff\xff\xff\xff\xff\xff\xff\x9c"),
            (-100, false)
        );
        // More than 8 significant bytes overflows even in a 12-byte field.
        assert!(octal(b"\xff\xff\xf8\x89\x67\x1c\x5d\x1e\x77\x26\xa1\x9f").1);
        assert_eq!(
            octal(b"\x80\x7f\xff\xff\xff\xff\xff\xff\xff"),
            (i64::MAX, false)
        );
        assert!(octal(b"\x80\x80\x00\x00\x00\x00\x00\x00\x00").1);
        assert_eq!(
            octal(b"\xff\x80\x00\x00\x00\x00\x00\x00\x00"),
            (i64::MIN, false)
        );
        assert!(octal(b"\xff\x7f\xff\xff\xff\xff\xff\xff\xff").1);
    }

    // --- writer/reader round-trip over edge-case headers ---

    #[test]
    fn roundtrip_edge_headers() {
        let long = "d".repeat(60);
        let deep = format!("{long}/{long}/name-over-100-bytes"); // splittable ASCII
        let cases: Vec<TarHeader> = vec![
            {
                let mut h = pax_header("é-utf8".as_bytes(), TYPE_REG);
                h.mode = 0o644;
                h.mod_time = Some(PaxTime::unix(1, 0));
                h
            },
            {
                let mut h = pax_header(deep.as_bytes(), TYPE_REG);
                h.mode = 0o600;
                h.mod_time = Some(PaxTime::unix(1_500_000_000, 123_456_789));
                h
            },
            {
                let mut h = pax_header(b"big-ids", TYPE_REG);
                h.mode = 0o644;
                h.uid = 4294967294;
                h.gid = 4294967294;
                h.mod_time = Some(PaxTime::unix(0, 0));
                h
            },
            {
                let mut h = pax_header(b"old", TYPE_REG);
                h.mode = 0o644;
                h.mod_time = Some(PaxTime::unix(0, -1));
                h
            },
            {
                let mut h = pax_header(b"far-future", TYPE_REG);
                h.mode = 0o644;
                h.mod_time = Some(PaxTime::unix(1 << 34, 0));
                h
            },
            {
                let mut h = pax_header(b"ln", TYPE_SYMLINK);
                h.mode = 0o777;
                h.linkname = "→/".repeat(40).into_bytes(); // long non-ASCII linkname
                h.mod_time = Some(PaxTime::unix(2, 0));
                h
            },
            {
                let mut h = pax_header(b"xattrs", TYPE_REG);
                h.mode = 0o600;
                h.mod_time = Some(PaxTime::unix(3, 7));
                h.pax_records
                    .insert(b"SCHILY.xattr.user.bin".to_vec(), vec![0u8, 1, 2, 255, 254]);
                h.pax_records
                    .insert(b"SCHILY.xattr.user.empty".to_vec(), Vec::new());
                h
            },
        ];

        let mut buf: Vec<u8> = Vec::new();
        let mut tw = TarWriter::new(&mut buf);
        for h in &cases {
            tw.write_header(h).unwrap();
        }
        tw.close().unwrap();

        let mut r = &buf[..];
        let mut tr = TarReader::new(&mut r);
        for want in &cases {
            let got = tr.next().expect("read").expect("entry");
            assert_eq!(got.name, want.name, "name");
            assert_eq!(got.typeflag, want.typeflag);
            assert_eq!(got.linkname, want.linkname);
            assert_eq!(got.uid, want.uid);
            assert_eq!(got.gid, want.gid);
            assert_eq!(got.mode, want.mode);
            assert_eq!(got.mod_time, want.mod_time, "{:?}", want.name);
            // Non-empty xattr records survive; the empty-valued one is
            // present as a record (merge keeps records verbatim).
            for (k, v) in &want.pax_records {
                assert_eq!(got.pax_records.get(k), Some(v), "record {k:?}");
            }
        }
        assert!(tr.next().unwrap().is_none());
    }
}
