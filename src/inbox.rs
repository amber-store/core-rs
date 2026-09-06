//! Durable pack receiving: stores authenticated packs that have been received
//! but not yet processed into the packstore. An entry is a single
//! self-describing file: a CBOR meta header framed by its big-endian length,
//! followed by the raw amberpack body. Entries are content-addressed by BLAKE3
//! of the body, so a re-received identical pack is idempotent. A pool of
//! workers drains the directory into the store; setting a reference waits on
//! the entries tagged with that root. The directory is the only durable state
//! — on restart a scan rebuilds the in-memory view and resumes processing.
//!
//! This is a semantic port of Go's `inbox` package (`inbox.go` + `entry.go`).
//! The meta header encoding is byte-identical to Go's (fxamacker/cbor core
//! deterministic mode with `NilContainerAsEmpty`); the decoder accepts the
//! lax input surface of fxamacker's default decode mode (pinned corner cases
//! and the few divergences are listed in `port-notes/inbox.md`). Go's
//! `*slog.Logger` parameter is ported as an optional [`LogFn`] callback;
//! `None` discards, like Go's nil logger.

use std::collections::{HashMap, VecDeque};
use std::ffi::{OsStr, OsString};
use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex, PoisonError};
use std::thread::{self, JoinHandle};

use crate::amberpack;
use crate::cbor::{
    MAJOR_ARRAY, MAJOR_BSTR, MAJOR_MAP, MAJOR_NEGINT, MAJOR_TSTR, MAJOR_UINT, append_bstr,
    append_head,
};
use crate::key::{self, Key};
use crate::packstore::{self, Store, WriteOpts};

/// CBOR major type 6 (the shared [`crate::cbor`] helpers have no callers for
/// it elsewhere, so the constant lives here).
const MAJOR_TAG: u8 = 6;

// ---------------------------------------------------------------------------
// Meta header codec (Go: entry.go)
// ---------------------------------------------------------------------------

/// Tags a staged pack with the (ref, root) it belongs to and when it arrived.
/// The barrier keys on `root` alone; `ref_` and `received_at` are carried for
/// operability (which ref a pending pack was for, how old it is) (Go: `Meta`).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Meta {
    /// The reference name (CBOR map key 0; Go: `Ref`).
    pub ref_: String,
    /// The 32-byte root key (CBOR map key 1; Go: `Root`). Untrusted until it
    /// passes [`Key::parse`].
    pub root: Vec<u8>,
    /// Nanoseconds since the Unix epoch (CBOR map key 2; Go: `ReceivedAt`).
    pub received_at: i64,
}

/// Encodes `m` exactly as Go's deterministic CBOR mode does (fxamacker
/// `CoreDetEncOptions` + `NilContainerAsEmpty`): a 3-entry map with integer
/// keys 0/1/2 ascending, shortest-form heads, `ref_` as a text string, `root`
/// as a byte string (Go's nil slice encodes as the empty byte string, which
/// is also what an empty `Vec` yields here).
fn encode_meta(m: &Meta) -> Vec<u8> {
    let mut b = Vec::with_capacity(11 + m.ref_.len() + m.root.len());
    b.push((MAJOR_MAP << 5) | 3);
    b.push(0x00);
    append_head(&mut b, MAJOR_TSTR, m.ref_.len() as u64);
    b.extend_from_slice(m.ref_.as_bytes());
    b.push(0x01);
    append_bstr(&mut b, &m.root);
    b.push(0x02);
    if m.received_at >= 0 {
        append_head(&mut b, MAJOR_UINT, m.received_at as u64);
    } else {
        // CBOR encodes a negative integer v as the argument -1-v, which for
        // v < 0 is the bitwise complement of its two's-complement bits.
        append_head(&mut b, MAJOR_NEGINT, !(m.received_at as u64));
    }
    b
}

/// Writes `[u32 BE meta-len][meta CBOR]` to `w` (Go: `writeMetaHeader`).
fn write_meta_header<W: Write>(w: &mut W, m: &Meta) -> io::Result<()> {
    let b = encode_meta(m);
    w.write_all(&(b.len() as u32).to_be_bytes())?;
    w.write_all(&b)
}

/// An error from [`read_meta_header`], carrying Go's wrapping prefixes.
#[derive(Debug, thiserror::Error)]
enum MetaError {
    /// The 4-byte length prefix could not be read.
    #[error("reading inbox meta length: {0}")]
    Length(io::Error),
    /// The meta bytes could not be read in full.
    #[error("reading inbox meta: {0}")]
    Body(io::Error),
    /// The meta bytes are not a decodable CBOR `Meta`.
    #[error("decoding inbox meta: {0}")]
    Decode(DecodeError),
}

/// A CBOR decoding failure; the message carries the same diagnostic detail as
/// fxamacker's errors (the exact text differs — see `port-notes/inbox.md`).
#[derive(Debug, thiserror::Error)]
#[error("{0}")]
struct DecodeError(String);

/// Reads a header written by [`write_meta_header`] and leaves `r` positioned
/// at the first body byte (Go: `readMetaHeader`).
fn read_meta_header<R: Read>(r: &mut R) -> Result<Meta, MetaError> {
    let mut len_buf = [0u8; 4];
    r.read_exact(&mut len_buf).map_err(MetaError::Length)?;
    let mut buf = vec![0u8; u32::from_be_bytes(len_buf) as usize];
    r.read_exact(&mut buf).map_err(MetaError::Body)?;
    decode_meta(&buf).map_err(MetaError::Decode)
}

/// fxamacker's default maximum nesting depth (`MaxNestedLevelsDefault`); the
/// top-level item is level 1, so a chain of 33 nested containers fails.
const MAX_NESTED: usize = 32;

/// One decoded CBOR head. Major 7 splits into [`Head::Simple`] (additional
/// info 0–24, including false/true/null/undefined) and [`Head::Float`]
/// (additional info 25–27; the bits are never needed, so they are dropped)
/// because a float's payload could otherwise be mistaken for a simple value.
enum Head {
    /// A definite head of major type 0–6 with its argument.
    Val(u8, u64),
    /// An indefinite-length start for major type 2–5.
    Indef(u8),
    /// A simple value (major 7).
    Simple(u8),
    /// A half/single/double float (major 7, payload consumed).
    Float,
    /// The `0xff` "break" code.
    Break,
}

/// The fxamacker-style name of a head's type, for diagnostics.
fn head_type(h: &Head) -> &'static str {
    match h {
        Head::Val(MAJOR_UINT, _) => "positive integer",
        Head::Val(MAJOR_NEGINT, _) => "negative integer",
        Head::Val(MAJOR_BSTR, _) | Head::Indef(MAJOR_BSTR) => "byte string",
        Head::Val(MAJOR_TSTR, _) | Head::Indef(MAJOR_TSTR) => "UTF-8 text string",
        Head::Val(MAJOR_ARRAY, _) | Head::Indef(MAJOR_ARRAY) => "array",
        Head::Val(MAJOR_MAP, _) | Head::Indef(MAJOR_MAP) => "map",
        Head::Val(MAJOR_TAG, _) => "tag",
        Head::Simple(_) | Head::Float => "primitives",
        Head::Break => "break",
        Head::Val(..) | Head::Indef(..) => "invalid",
    }
}

fn derr(msg: impl Into<String>) -> DecodeError {
    DecodeError(msg.into())
}

/// Decoder state over one length-framed meta buffer.
struct Dec<'a> {
    b: &'a [u8],
    pos: usize,
}

impl<'a> Dec<'a> {
    /// The truncation error: fxamacker reports `EOF` for empty input and
    /// `unexpected EOF` for input that ends mid-item.
    fn eof(&self) -> DecodeError {
        derr(if self.b.is_empty() {
            "EOF"
        } else {
            "unexpected EOF"
        })
    }

    fn byte(&mut self) -> Result<u8, DecodeError> {
        let v = *self.b.get(self.pos).ok_or_else(|| self.eof())?;
        self.pos += 1;
        Ok(v)
    }

    fn take(&mut self, n: u64) -> Result<&'a [u8], DecodeError> {
        if n > (self.b.len() - self.pos) as u64 {
            return Err(self.eof());
        }
        let s = &self.b[self.pos..self.pos + n as usize];
        self.pos += n as usize;
        Ok(s)
    }

    fn uint_arg(&mut self, size: usize) -> Result<u64, DecodeError> {
        let mut v: u64 = 0;
        for _ in 0..size {
            v = v << 8 | u64::from(self.byte()?);
        }
        Ok(v)
    }

    /// Reads one head. All definite-length forms are accepted, including
    /// non-shortest ones (fxamacker's default mode does not require
    /// preferred serialization). `0xf8` simple values below 32 are rejected
    /// as not well-formed, exactly like fxamacker.
    fn head(&mut self) -> Result<Head, DecodeError> {
        let first = self.byte()?;
        let (major, ai) = (first >> 5, first & 0x1f);
        if major == 7 {
            return match ai {
                0..=23 => Ok(Head::Simple(ai)),
                24 => {
                    let v = self.byte()?;
                    if v < 32 {
                        return Err(derr(format!(
                            "cbor: invalid simple value {v} for type primitives"
                        )));
                    }
                    Ok(Head::Simple(v))
                }
                25..=27 => {
                    self.uint_arg(1 << (ai - 24))?;
                    Ok(Head::Float)
                }
                31 => Ok(Head::Break),
                _ => Err(derr(format!(
                    "cbor: invalid additional information {ai} for type primitives"
                ))),
            };
        }
        let name = head_type(&Head::Val(major, 0));
        match ai {
            0..=23 => Ok(Head::Val(major, u64::from(ai))),
            24..=27 => Ok(Head::Val(major, self.uint_arg(1 << (ai - 24))?)),
            31 if (MAJOR_BSTR..=MAJOR_MAP).contains(&major) => Ok(Head::Indef(major)),
            _ => Err(derr(format!(
                "cbor: invalid additional information {ai} for type {name}"
            ))),
        }
    }

    /// Reads a head without consuming it.
    fn peek_head(&mut self) -> Result<Head, DecodeError> {
        let save = self.pos;
        let h = self.head();
        self.pos = save;
        h
    }

    /// Consumes a pending `0xff` break, reporting whether one was present.
    fn peek_break(&mut self) -> Result<bool, DecodeError> {
        match self.b.get(self.pos) {
            None => Err(self.eof()),
            Some(0xff) => {
                self.pos += 1;
                Ok(true)
            }
            Some(_) => Ok(false),
        }
    }

    fn check_depth(&self, depth: usize) -> Result<(), DecodeError> {
        if depth > MAX_NESTED {
            return Err(derr(format!(
                "cbor: exceeded max nested level {MAX_NESTED}"
            )));
        }
        Ok(())
    }

    /// Reads the chunks of an indefinite-length string of major type `major`
    /// into one buffer. Chunks must be definite-length strings of the same
    /// major type (RFC 8949 well-formedness, enforced by fxamacker).
    fn chunks(&mut self, major: u8) -> Result<Vec<u8>, DecodeError> {
        let name = head_type(&Head::Indef(major));
        let mut out = Vec::new();
        loop {
            if self.peek_break()? {
                return Ok(out);
            }
            match self.head()? {
                Head::Val(mj, n) if mj == major => out.extend_from_slice(self.take(n)?),
                Head::Indef(mj) if mj == major => {
                    return Err(derr(format!(
                        "cbor: indefinite-length {name} chunk is not definite-length"
                    )));
                }
                h => {
                    return Err(derr(format!(
                        "cbor: wrong element type {} for indefinite-length {name}",
                        head_type(&h)
                    )));
                }
            }
        }
    }

    /// Skips one whole item starting at `h`, checking only well-formedness
    /// (no UTF-8 validation, no tag-content checks — fxamacker applies those
    /// only when decoding into a value). `depth` is the item's own nesting
    /// level.
    fn skip_from(&mut self, h: Head, depth: usize) -> Result<(), DecodeError> {
        match h {
            Head::Val(MAJOR_UINT | MAJOR_NEGINT, _) | Head::Simple(_) | Head::Float => Ok(()),
            Head::Val(MAJOR_BSTR | MAJOR_TSTR, n) => self.take(n).map(|_| ()),
            Head::Indef(mj @ (MAJOR_BSTR | MAJOR_TSTR)) => self.chunks(mj).map(|_| ()),
            Head::Val(MAJOR_ARRAY, n) => {
                self.check_depth(depth)?;
                for _ in 0..n {
                    self.skip_item(depth + 1)?;
                }
                Ok(())
            }
            Head::Indef(MAJOR_ARRAY) => {
                self.check_depth(depth)?;
                while !self.peek_break()? {
                    self.skip_item(depth + 1)?;
                }
                Ok(())
            }
            Head::Val(MAJOR_MAP, n) => {
                self.check_depth(depth)?;
                for _ in 0..n {
                    self.skip_item(depth + 1)?;
                    self.skip_item(depth + 1)?;
                }
                Ok(())
            }
            Head::Indef(MAJOR_MAP) => {
                self.check_depth(depth)?;
                while !self.peek_break()? {
                    self.skip_item(depth + 1)?;
                    self.skip_item(depth + 1)?;
                }
                Ok(())
            }
            Head::Val(MAJOR_TAG, _) => {
                self.check_depth(depth)?;
                self.skip_item(depth + 1)
            }
            Head::Break => Err(derr("cbor: unexpected \"break\" code")),
            Head::Val(..) | Head::Indef(..) => Err(derr("cbor: invalid head")),
        }
    }

    fn skip_item(&mut self, depth: usize) -> Result<(), DecodeError> {
        let h = self.head()?;
        self.skip_from(h, depth)
    }

    /// Mirrors fxamacker's content-type validation for the registered tags 0
    /// (standard date/time string) and 1 (epoch date/time); other tag numbers
    /// pass through untouched.
    fn check_tag_content(&mut self, tag: u64) -> Result<(), DecodeError> {
        if tag > 1 {
            return Ok(());
        }
        let h = self.peek_head()?;
        match (tag, &h) {
            (0, Head::Val(MAJOR_TSTR, _) | Head::Indef(MAJOR_TSTR)) => Ok(()),
            (0, _) => Err(derr(format!(
                "cbor: tag number 0 must be followed by text string, got {}",
                head_type(&h)
            ))),
            (_, Head::Val(MAJOR_UINT | MAJOR_NEGINT, _) | Head::Float) => Ok(()),
            (_, _) => Err(derr(format!(
                "cbor: tag number 1 must be followed by integer or floating-point number, got {}",
                head_type(&h)
            ))),
        }
    }

    /// Decodes the value for field 0 (`ref_`): a text string; null/undefined
    /// yields the zero value; tags are unwrapped.
    fn decode_ref(&mut self, depth: usize) -> Result<String, DecodeError> {
        match self.head()? {
            Head::Val(MAJOR_TSTR, n) => utf8_owned(self.take(n)?),
            Head::Indef(MAJOR_TSTR) => utf8_owned(&self.chunks(MAJOR_TSTR)?),
            Head::Simple(22 | 23) => Ok(String::new()),
            Head::Val(MAJOR_TAG, t) => {
                self.check_depth(depth)?;
                self.check_tag_content(t)?;
                self.decode_ref(depth + 1)
            }
            h => Err(derr(format!(
                "cbor: cannot unmarshal {} into Meta field 0 of type string",
                head_type(&h)
            ))),
        }
    }

    /// Decodes the value for field 1 (`root`): a byte string, or — matching
    /// fxamacker's reflection-driven laxness — an array of integers 0–255;
    /// null/undefined yields the empty value; tags are unwrapped.
    fn decode_root(&mut self, depth: usize) -> Result<Vec<u8>, DecodeError> {
        match self.head()? {
            Head::Val(MAJOR_BSTR, n) => Ok(self.take(n)?.to_vec()),
            Head::Indef(MAJOR_BSTR) => self.chunks(MAJOR_BSTR),
            Head::Val(MAJOR_ARRAY, n) => {
                self.check_depth(depth)?;
                let mut out = Vec::new();
                for _ in 0..n {
                    out.push(self.u8_elem()?);
                }
                Ok(out)
            }
            Head::Indef(MAJOR_ARRAY) => {
                self.check_depth(depth)?;
                let mut out = Vec::new();
                while !self.peek_break()? {
                    out.push(self.u8_elem()?);
                }
                Ok(out)
            }
            Head::Simple(22 | 23) => Ok(Vec::new()),
            Head::Val(MAJOR_TAG, t) => {
                self.check_depth(depth)?;
                self.check_tag_content(t)?;
                self.decode_root(depth + 1)
            }
            h => Err(derr(format!(
                "cbor: cannot unmarshal {} into Meta field 1 of type []uint8",
                head_type(&h)
            ))),
        }
    }

    /// One byte-sized element of an array decoded into `root`
    /// (null/undefined decodes to 0, like fxamacker).
    fn u8_elem(&mut self) -> Result<u8, DecodeError> {
        match self.head()? {
            Head::Val(MAJOR_UINT, v) if v <= 255 => Ok(v as u8),
            Head::Simple(22 | 23) => Ok(0),
            Head::Val(MAJOR_UINT, v) => Err(derr(format!(
                "cbor: cannot unmarshal positive integer into Meta field 1 of type uint8 ({v} overflows uint8)"
            ))),
            Head::Break => Err(derr("cbor: unexpected \"break\" code")),
            h => Err(derr(format!(
                "cbor: cannot unmarshal {} into Meta field 1 of type uint8",
                head_type(&h)
            ))),
        }
    }

    /// Decodes the value for field 2 (`received_at`): an integer within the
    /// i64 range; null/undefined yields 0; tags are unwrapped.
    fn decode_at(&mut self, depth: usize) -> Result<i64, DecodeError> {
        match self.head()? {
            Head::Val(MAJOR_UINT, n) => i64::try_from(n).map_err(|_| {
                derr(format!(
                    "cbor: cannot unmarshal positive integer into Meta field 2 of type int64 ({n} overflows int64)"
                ))
            }),
            Head::Val(MAJOR_NEGINT, n) => match i64::try_from(n) {
                Ok(v) => Ok(-1 - v),
                Err(_) => Err(derr(format!(
                    "cbor: cannot unmarshal negative integer into Meta field 2 of type int64 (-{} overflows int64)",
                    (n as u128) + 1
                ))),
            },
            Head::Simple(22 | 23) => Ok(0),
            Head::Val(MAJOR_TAG, t) => {
                self.check_depth(depth)?;
                self.check_tag_content(t)?;
                self.decode_at(depth + 1)
            }
            h => Err(derr(format!(
                "cbor: cannot unmarshal {} into Meta field 2 of type int64",
                head_type(&h)
            ))),
        }
    }

    /// Decodes one map entry. Keys 0/1/2 populate their field on first
    /// occurrence; duplicate keys are skipped without semantic checks (Go's
    /// `DupMapKeyQuiet` keeps the first value); other integer and text keys
    /// are quietly skipped; remaining key types are errors.
    fn entry(&mut self, m: &mut Meta, set: &mut [bool; 3]) -> Result<(), DecodeError> {
        // The top-level map is nesting level 1, so its keys/values sit at 2.
        const VDEPTH: usize = 2;
        match self.head()? {
            Head::Val(MAJOR_UINT, k) if k <= 2 => {
                let k = k as usize;
                if set[k] {
                    return self.skip_item(VDEPTH);
                }
                set[k] = true;
                match k {
                    0 => m.ref_ = self.decode_ref(VDEPTH)?,
                    1 => m.root = self.decode_root(VDEPTH)?,
                    _ => m.received_at = self.decode_at(VDEPTH)?,
                }
                Ok(())
            }
            Head::Val(MAJOR_UINT | MAJOR_NEGINT, _) => self.skip_item(VDEPTH),
            Head::Val(MAJOR_TSTR, n) => {
                utf8_owned(self.take(n)?)?;
                self.skip_item(VDEPTH)
            }
            Head::Indef(MAJOR_TSTR) => {
                utf8_owned(&self.chunks(MAJOR_TSTR)?)?;
                self.skip_item(VDEPTH)
            }
            Head::Break => Err(derr("cbor: unexpected \"break\" code")),
            h => Err(derr(format!(
                "cbor: map key is of type {} and cannot be used to match struct field name",
                head_type(&h)
            ))),
        }
    }
}

fn utf8_owned(b: &[u8]) -> Result<String, DecodeError> {
    String::from_utf8(b.to_vec()).map_err(|_| derr("cbor: invalid UTF-8 string"))
}

/// Decodes one `Meta` the way Go's `cbor.Unmarshal` (default decode mode)
/// does: a map (definite or indefinite) with the acceptance rules pinned in
/// the tests below, or null/undefined for the zero value; trailing bytes are
/// rejected.
fn decode_meta(buf: &[u8]) -> Result<Meta, DecodeError> {
    let mut d = Dec { b: buf, pos: 0 };
    let mut m = Meta::default();
    let mut set = [false; 3];
    match d.head()? {
        Head::Simple(22 | 23) => {}
        Head::Val(MAJOR_MAP, n) => {
            for _ in 0..n {
                d.entry(&mut m, &mut set)?;
            }
        }
        Head::Indef(MAJOR_MAP) => {
            while !d.peek_break()? {
                d.entry(&mut m, &mut set)?;
            }
        }
        Head::Break => return Err(derr("cbor: unexpected \"break\" code")),
        h => {
            return Err(derr(format!(
                "cbor: cannot unmarshal {} into Meta",
                head_type(&h)
            )));
        }
    }
    if d.pos != buf.len() {
        return Err(derr(format!(
            "cbor: {} bytes of extraneous data starting at index {}",
            buf.len() - d.pos,
            d.pos
        )));
    }
    Ok(m)
}

// ---------------------------------------------------------------------------
// Inbox (Go: inbox.go)
// ---------------------------------------------------------------------------

/// An error log callback: receives one preformatted line per event, e.g.
/// `inbox: entry failed processing, quarantining name=<file> error=<cause>`.
/// This ports Go's `*slog.Logger` parameter (every call site logs at Error
/// level with `name`/`error` attributes, which are appended as `key=value`).
pub type LogFn = Box<dyn Fn(&str) + Send + Sync>;

/// Receives packs, persists them durably, and drains them into a packstore
/// from a worker pool. It is safe for concurrent use (Go: `Inbox`).
pub struct Inbox {
    inner: Arc<Shared>,
    workers: Mutex<Vec<JoinHandle<()>>>,
}

/// State shared with the worker threads.
struct Shared {
    dir: PathBuf,
    tmp_dir: PathBuf,
    fail_dir: PathBuf,
    store: Arc<Store>,
    log: Option<LogFn>,
    state: Mutex<State>,
    cond: Condvar,
}

/// Fields Go guards with `mu` (the queue, the per-root barrier counts, and
/// the closed flag).
#[derive(Default)]
struct State {
    work: VecDeque<WorkItem>,
    /// root -> count of unprocessed entries.
    groups: HashMap<Key, usize>,
    closed: bool,
}

struct WorkItem {
    name: OsString,
    root: Key,
}

/// An error reading one entry's meta header (open + header + root parse);
/// only ever logged (Go: `readRoot`'s error).
#[derive(Debug, thiserror::Error)]
enum EntryError {
    #[error(transparent)]
    Io(#[from] io::Error),
    #[error(transparent)]
    Meta(#[from] MetaError),
    #[error(transparent)]
    Key(#[from] key::Error),
}

impl Inbox {
    /// Prepares the inbox directory tree, recovers entries left by a previous
    /// run, and starts `workers` processing threads. `workers == 0` means the
    /// available parallelism (Go: `workers <= 0` means `GOMAXPROCS(0)`). A
    /// `None` log discards (Go: nil `*slog.Logger`).
    pub fn open(
        dir: impl AsRef<Path>,
        store: Arc<Store>,
        workers: usize,
        log: Option<LogFn>,
    ) -> io::Result<Inbox> {
        let dir = dir.as_ref().to_path_buf();
        let workers = if workers == 0 {
            thread::available_parallelism().map_or(1, |n| n.get())
        } else {
            workers
        };
        let inner = Arc::new(Shared {
            tmp_dir: dir.join("tmp"),
            fail_dir: dir.join("failed"),
            dir,
            store,
            log,
            state: Mutex::new(State::default()),
            cond: Condvar::new(),
        });
        for d in [&inner.dir, &inner.tmp_dir, &inner.fail_dir] {
            let mut b = fs::DirBuilder::new();
            b.recursive(true).mode(0o755);
            b.create(d)?;
        }
        inner.recover()?;
        let ib = Inbox {
            inner: Arc::clone(&inner),
            workers: Mutex::new(Vec::with_capacity(workers)),
        };
        for _ in 0..workers {
            let inner = Arc::clone(&inner);
            let spawned = thread::Builder::new()
                .name("inbox-worker".into())
                .spawn(move || inner.process_loop());
            match spawned {
                Ok(h) => unpoison(ib.workers.lock()).push(h),
                Err(e) => {
                    ib.close(); // join whatever was spawned
                    return Err(e);
                }
            }
        }
        Ok(ib)
    }

    /// Writes `meta` and streams `body` into a fresh tmp file, returning the
    /// tmp path, BLAKE3 of the body bytes (only the body feeds the hash), and
    /// the body length. The caller authorizes the request against the hash,
    /// then calls [`Inbox::commit`] or [`Inbox::discard`] (Go: `Stage`).
    pub fn stage(&self, meta: &Meta, mut body: impl Read) -> io::Result<(PathBuf, [u8; 32], u64)> {
        let (mut f, tmp_path) = create_temp(&self.inner.tmp_dir)?;
        match stage_into(&mut f, meta, &mut body) {
            Ok((hash, n)) => {
                drop(f);
                Ok((tmp_path, hash, n))
            }
            Err(e) => {
                drop(f);
                let _ = fs::remove_file(&tmp_path);
                Err(e)
            }
        }
    }

    /// Removes a staged tmp file (authorization failed or oversize) (Go:
    /// `Discard`).
    pub fn discard(&self, tmp_path: &Path) {
        let _ = fs::remove_file(tmp_path);
    }

    /// Publishes a staged tmp file under its content-addressed name and
    /// enqueues it. It is idempotent: if an entry with the same body already
    /// exists, the tmp file is discarded and `false` is returned. `root`
    /// updates the barrier accounting and must equal the root staged into the
    /// file (Go: `Commit`).
    pub fn commit(&self, tmp_path: &Path, body_hash: &[u8], root: Key) -> io::Result<bool> {
        let name = format!("{}.pack", hex_string(body_hash));
        let dst = self.inner.dir.join(&name);
        match fs::metadata(&dst) {
            Ok(_) => {
                self.discard(tmp_path);
                return Ok(false);
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => return Err(e),
        }
        fs::rename(tmp_path, &dst)?;
        sync_dir(&self.inner.dir)?;
        let mut st = unpoison(self.inner.state.lock());
        *st.groups.entry(root).or_insert(0) += 1;
        st.work.push_back(WorkItem {
            name: name.into(),
            root,
        });
        self.inner.cond.notify_all();
        Ok(true)
    }

    /// Blocks until no entries tagged with `root` remain unprocessed. With an
    /// empty group it returns immediately (Go: `WaitFor`).
    pub fn wait_for(&self, root: Key) {
        let mut st = unpoison(self.inner.state.lock());
        while st.groups.get(&root).copied().unwrap_or(0) > 0 {
            st = unpoison(self.inner.cond.wait(st));
        }
    }

    /// Stops accepting new work, drains what is already queued, and waits for
    /// the processing threads to exit. Staged-but-uncommitted tmp files are
    /// left for the next [`Inbox::open`] to sweep (Go: `Close`, whose error
    /// is always nil; dropping the `Inbox` closes it too).
    pub fn close(&self) {
        {
            let mut st = unpoison(self.inner.state.lock());
            st.closed = true;
            self.inner.cond.notify_all();
        }
        let handles = std::mem::take(&mut *unpoison(self.workers.lock()));
        for h in handles {
            let _ = h.join();
        }
    }
}

impl Drop for Inbox {
    fn drop(&mut self) {
        self.close();
    }
}

impl Shared {
    fn log_error(&self, msg: &str, name: &OsStr, err: &dyn fmt::Display) {
        if let Some(log) = &self.log {
            log(&format!(
                "{msg} name={} error={err}",
                name.to_string_lossy()
            ));
        }
    }

    /// Sweeps partial transfers and enqueues committed entries from a
    /// previous run (Go: `recover`).
    fn recover(&self) -> io::Result<()> {
        for e in fs::read_dir(&self.tmp_dir)? {
            let p = e?.path();
            // Go's os.Remove unlinks files and removes empty directories.
            let _ = fs::remove_file(&p).or_else(|_| fs::remove_dir(&p));
        }
        // Go's os.ReadDir returns entries sorted by filename.
        let mut entries: Vec<(OsString, bool)> = Vec::new();
        for e in fs::read_dir(&self.dir)? {
            let e = e?;
            let is_dir = e.file_type().map(|t| t.is_dir()).unwrap_or(false);
            entries.push((e.file_name(), is_dir));
        }
        entries.sort();
        let mut st = unpoison(self.state.lock());
        for (name, is_dir) in entries {
            if is_dir || !name.as_encoded_bytes().ends_with(b".pack") {
                continue;
            }
            match self.read_root(&name) {
                Ok(root) => {
                    *st.groups.entry(root).or_insert(0) += 1;
                    st.work.push_back(WorkItem { name, root });
                }
                Err(err) => {
                    self.log_error(
                        "inbox: unreadable entry on recovery, quarantining",
                        &name,
                        &err,
                    );
                    let _ = fs::rename(self.dir.join(&name), self.fail_dir.join(&name));
                }
            }
        }
        Ok(())
    }

    /// Reads just the meta header of an entry and returns its root key (Go:
    /// `readRoot`).
    fn read_root(&self, name: &OsStr) -> Result<Key, EntryError> {
        let mut f = File::open(self.dir.join(name))?;
        let m = read_meta_header(&mut f)?;
        Ok(Key::parse(&m.root)?)
    }

    /// One worker: pop, process, release the barrier count (Go:
    /// `processLoop`).
    fn process_loop(&self) {
        loop {
            let mut st = unpoison(self.state.lock());
            while st.work.is_empty() && !st.closed {
                st = unpoison(self.cond.wait(st));
            }
            let Some(item) = st.work.pop_front() else {
                return; // queue empty and closed
            };
            drop(st);

            self.process(&item.name);

            let mut st = unpoison(self.state.lock());
            if let Some(n) = st.groups.get_mut(&item.root) {
                if *n > 1 {
                    *n -= 1;
                } else {
                    st.groups.remove(&item.root);
                }
            }
            self.cond.notify_all();
        }
    }

    /// Ingests one entry into the store. On success the file is removed; on a
    /// decode/verify error it is quarantined under `failed/` (Go: `process`).
    fn process(&self, name: &OsStr) {
        let path = self.dir.join(name);
        let mut f = match File::open(&path) {
            Ok(f) => f,
            Err(e) => {
                self.log_error("inbox: opening entry failed", name, &e);
                return;
            }
        };
        if let Err(e) = read_meta_header(&mut f) {
            drop(f);
            self.quarantine(name, &e);
            return;
        }
        let rd = amberpack::Reader::new(f); // positioned at the body
        let seq = rd.map(|r| {
            r.map(|(key, data)| packstore::Object {
                key,
                data,
                record: None,
            })
        });
        let (_, res) = self.store.write_parallel(
            seq,
            WriteOpts {
                verify: true,
                ..Default::default()
            },
        );
        if let Err(werr) = res {
            self.quarantine(name, &werr);
            return;
        }
        if let Err(e) = fs::remove_file(&path) {
            self.log_error("inbox: removing processed entry failed", name, &e);
        }
    }

    fn quarantine(&self, name: &OsStr, cause: &dyn fmt::Display) {
        self.log_error("inbox: entry failed processing, quarantining", name, cause);
        if let Err(e) = fs::rename(self.dir.join(name), self.fail_dir.join(name)) {
            self.log_error("inbox: quarantine rename failed", name, &e);
        }
    }
}

/// Writes the header and streams the body, hashing only the body bytes, then
/// fsyncs (the body of Go's `Stage` between `CreateTemp` and `Close`).
fn stage_into(f: &mut File, meta: &Meta, body: &mut dyn Read) -> io::Result<([u8; 32], u64)> {
    write_meta_header(f, meta)?;
    let mut h = blake3::Hasher::new();
    let mut n: u64 = 0;
    let mut buf = [0u8; 32 * 1024];
    loop {
        let r = match body.read(&mut buf) {
            Ok(0) => break,
            Ok(r) => r,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        };
        f.write_all(&buf[..r])?;
        h.update(&buf[..r]);
        n += r as u64;
    }
    f.sync_all()?;
    Ok((*h.finalize().as_bytes(), n))
}

/// Creates a fresh exclusive file in `dir` (Go: `os.CreateTemp(dir,
/// "stage-*")`): a random name, mode 0600, retried on collision.
fn create_temp(dir: &Path) -> io::Result<(File, PathBuf)> {
    use std::hash::{BuildHasher, Hasher, RandomState};
    for attempt in 0..10_000u32 {
        let mut h = RandomState::new().build_hasher();
        h.write_u32(attempt);
        let path = dir.join(format!("stage-{}", h.finish() as u32));
        match OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&path)
        {
            Ok(f) => return Ok((f, path)),
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e),
        }
    }
    Err(io::Error::new(
        io::ErrorKind::AlreadyExists,
        "inbox: too many staging temp-file collisions",
    ))
}

/// Fsyncs a directory so a rename into it is durable (Go: `syncDir`).
fn sync_dir(dir: &Path) -> io::Result<()> {
    File::open(dir)?.sync_all()
}

fn hex_string(b: &[u8]) -> String {
    use fmt::Write as _;
    let mut s = String::with_capacity(b.len() * 2);
    for &x in b {
        let _ = write!(s, "{x:02x}");
    }
    s
}

/// Recovers the guarded value from a poisoned lock: a panicking worker must
/// not wedge every other user of the inbox.
fn unpoison<T>(r: Result<T, PoisonError<T>>) -> T {
    r.unwrap_or_else(PoisonError::into_inner)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::key::Type;
    use crate::packstore::Options;
    use std::io::Cursor;

    fn hx(s: &str) -> Vec<u8> {
        hex::decode(s).unwrap()
    }

    // -- entry.go: Meta header codec ------------------------------------

    /// Go: TestMetaHeaderRoundTrip.
    #[test]
    fn meta_header_round_trip() {
        for at in [1234567890i64, -1, i64::MIN, i64::MAX] {
            let m = Meta {
                ref_: "site".into(),
                root: (0..32).collect(),
                received_at: at,
            };
            let mut buf = Vec::new();
            write_meta_header(&mut buf, &m).unwrap();
            // Append a fake body; read_meta_header must stop right after the
            // header.
            buf.extend_from_slice(b"BODYBYTES");
            let mut r = Cursor::new(&buf);
            let out = read_meta_header(&mut r).unwrap();
            assert_eq!(out, m);
            let mut rest = Vec::new();
            r.read_to_end(&mut rest).unwrap();
            assert_eq!(rest, b"BODYBYTES");
        }
    }

    /// The CBOR bytes are pinned against Go's encoder (fxamacker
    /// CoreDetEncOptions + NilContainerAsEmpty, via a throwaway Go harness
    /// that also cross-checked the real unexported `writeMetaHeader`).
    #[test]
    fn meta_encoding_matches_go() {
        let root_hex = "000102030405060708090a0b0c0d0e0f101112131415161718191a1b1c1d1e1f";
        let root: Vec<u8> = (0..32).collect();
        let m = |r: &str, root: Vec<u8>, at: i64| Meta {
            ref_: r.into(),
            root,
            received_at: at,
        };
        let cases: Vec<(Meta, String)> = vec![
            (
                m("site", root.clone(), 1234567890),
                format!("a3006473697465015820{root_hex}021a499602d2"),
            ),
            (Meta::default(), "a3006001400200".into()),
            (m("", vec![], -1), "a3006001400220".into()),
            (m("", vec![], -1234567890), "a300600140023a499602d1".into()),
            (
                m("r", vec![0xaa, 0xbb, 0xcc], 23),
                "a30061720143aabbcc0217".into(),
            ),
            (
                m("backup@nightly", root.clone(), 1754650000123456789),
                format!("a3006e6261636b7570406e696768746c79015820{root_hex}021b1859c4e0ea7f6d15"),
            ),
            (
                m("héllo→", vec![], 24),
                "a3006968c3a96c6c6fe286920140021818".into(),
            ),
            (
                m(&"a".repeat(300), vec![], 65536),
                format!("a30079012c{}0140021a00010000", "61".repeat(300)),
            ),
        ];
        for (meta, want) in &cases {
            assert_eq!(&hex_string(&encode_meta(meta)), want, "meta {meta:?}");
        }
        // The full framed header ([u32 BE len][CBOR]) as staged by Go.
        let mut framed = Vec::new();
        write_meta_header(&mut framed, &cases[0].0).unwrap();
        assert_eq!(
            hex_string(&framed),
            format!("00000030a3006473697465015820{root_hex}021a499602d2")
        );
    }

    /// Decode-acceptance corpus pinned against Go's `cbor.Unmarshal` (default
    /// DecMode) with the same throwaway harness. `Ok` carries the expected
    /// (ref, root-hex, received_at); `Err` a substring of the diagnostic.
    #[test]
    fn meta_decoding_matches_go() {
        let root_hex = "000102030405060708090a0b0c0d0e0f101112131415161718191a1b1c1d1e1f";
        enum Want {
            Ok(&'static str, String, i64),
            Err(&'static str),
        }
        use Want::*;
        let cases: Vec<(&str, String, Want)> = vec![
            // (name, input hex, expected)
            (
                "dup key: first wins",
                format!("a4006161006162015820{root_hex}021a499602d2"),
                Ok("a", root_hex.into(), 1234567890),
            ),
            (
                "unknown int key",
                "a2056161021818".into(),
                Ok("", String::new(), 24),
            ),
            (
                "negative key skipped",
                "a220616102182a".into(),
                Ok("", String::new(), 42),
            ),
            (
                "text key skipped",
                "a26178050218fa".into(),
                Ok("", String::new(), 250),
            ),
            ("null ref", "a100f6".into(), Ok("", String::new(), 0)),
            ("null root", "a101f6".into(), Ok("", String::new(), 0)),
            (
                "null received_at",
                "a102f6".into(),
                Ok("", String::new(), 0),
            ),
            ("undefined ref", "a100f7".into(), Ok("", String::new(), 0)),
            ("empty map", "a0".into(), Ok("", String::new(), 0)),
            ("null top level", "f6".into(), Ok("", String::new(), 0)),
            (
                "non-shortest field key",
                "a11800626869".into(),
                Ok("hi", String::new(), 0),
            ),
            (
                "indefinite map",
                "bf00626869ff".into(),
                Ok("hi", String::new(), 0),
            ),
            (
                "indefinite empty map",
                "bfff".into(),
                Ok("", String::new(), 0),
            ),
            (
                "indefinite text ref",
                "a1007f626869ff".into(),
                Ok("hi", String::new(), 0),
            ),
            (
                "indefinite bytes root",
                "a1015f42aabb41ccff".into(),
                Ok("", "aabbcc".into(), 0),
            ),
            (
                "tag 1 on received_at",
                "a102c11a499602d2".into(),
                Ok("", String::new(), 1234567890),
            ),
            (
                "tag 64 on root",
                format!("a101d8405820{}", "00".repeat(32)),
                Ok("", "00".repeat(32), 0),
            ),
            (
                "generic tag on root",
                "a101d9029741aa".into(),
                Ok("", "aa".into(), 0),
            ),
            (
                "nested generic tags on root",
                "a101d90297d9029841aa".into(),
                Ok("", "aa".into(), 0),
            ),
            (
                "array as root",
                "a10183010203".into(),
                Ok("", "010203".into(), 0),
            ),
            (
                "indefinite array as root",
                "a1019f0102ff".into(),
                Ok("", "0102".into(), 0),
            ),
            (
                "map under unknown key",
                "a205a26161016162020218fb".into(),
                Ok("", String::new(), 251),
            ),
            (
                "dup: null then value",
                "a300f6006474657374021817".into(),
                Ok("", String::new(), 23),
            ),
            (
                "dup: value then null",
                "a300647465737400f6021817".into(),
                Ok("test", String::new(), 23),
            ),
            (
                "dup: both set",
                "a2006161006162".into(),
                Ok("a", String::new(), 0),
            ),
            (
                "dup: ill-typed second",
                "a20061610005".into(),
                Ok("a", String::new(), 0),
            ),
            (
                "dup: float second",
                "a300616100f94100021821".into(),
                Ok("a", String::new(), 33),
            ),
            (
                "dup root: array then bytes",
                "a3018101014162021818".into(),
                Ok("", "01".into(), 24),
            ),
            (
                "invalid UTF-8 in skipped value",
                "a20562c32802181f".into(),
                Ok("", String::new(), 31),
            ),
            (
                "received_at at i64::MAX",
                "a1021b7fffffffffffffff".into(),
                Ok("", String::new(), i64::MAX),
            ),
            (
                "received_at at i64::MIN",
                "a1023b7fffffffffffffff".into(),
                Ok("", String::new(), i64::MIN),
            ),
            (
                "nesting depth 32 accepted",
                format!("a205{}8002182a", "81".repeat(30)),
                Ok("", String::new(), 42),
            ),
            (
                "null in root array",
                "a10182f600".into(),
                Ok("", "0000".into(), 0),
            ),
            (
                "tag 0 text into ref",
                "a100c06161".into(),
                Ok("a", String::new(), 0),
            ),
            (
                "zero-chunk indefinite root",
                "a1015fff".into(),
                Ok("", String::new(), 0),
            ),
            // Errors.
            ("trailing byte", "a0ff".into(), Err("extraneous data")),
            (
                "bool received_at",
                "a102f5".into(),
                Err("cannot unmarshal primitives into Meta field 2"),
            ),
            (
                "tag chain exceeds nesting in skipped value",
                format!("a205{}0002182a", "d90297".repeat(40)),
                Err("exceeded max nested level 32"),
            ),
            (
                "tag chain exceeds nesting in root",
                format!("a101{}41aa", "d90297".repeat(33)),
                Err("exceeded max nested level 32"),
            ),
            (
                "array top level",
                "820102".into(),
                Err("cannot unmarshal array into Meta"),
            ),
            (
                "uint top level",
                "17".into(),
                Err("cannot unmarshal positive integer into Meta"),
            ),
            (
                "byte string ref",
                "a1004163".into(),
                Err("cannot unmarshal byte string into Meta field 0"),
            ),
            (
                "uint root",
                "a10118ff".into(),
                Err("cannot unmarshal positive integer into Meta field 1"),
            ),
            (
                "float received_at",
                "a102f94100".into(),
                Err("cannot unmarshal primitives into Meta field 2"),
            ),
            (
                "received_at overflow",
                "a1021b8000000000000000".into(),
                Err("overflows int64"),
            ),
            (
                "received_at negative overflow",
                "a1023b8000000000000000".into(),
                Err("overflows int64"),
            ),
            (
                "reserved additional info",
                "a1021f".into(),
                Err("invalid additional information 31 for type positive integer"),
            ),
            (
                "map as root",
                "a101a10102".into(),
                Err("cannot unmarshal map into Meta field 1"),
            ),
            (
                "negative int in root array",
                "a1018120".into(),
                Err("cannot unmarshal negative integer into Meta field 1 of type uint8"),
            ),
            (
                "oversized int in root array",
                "a10181190100".into(),
                Err("overflows uint8"),
            ),
            (
                "invalid UTF-8 ref",
                "a10062c328".into(),
                Err("invalid UTF-8 string"),
            ),
            (
                "invalid UTF-8 in skipped key",
                "a262c3280502181e".into(),
                Err("invalid UTF-8 string"),
            ),
            (
                "byte string key",
                "a2417805021838".into(),
                Err("map key is of type byte string"),
            ),
            (
                "bool key",
                "a2f505021839".into(),
                Err("map key is of type primitives"),
            ),
            (
                "null key",
                "a2f60502183a".into(),
                Err("map key is of type primitives"),
            ),
            (
                "float key",
                "a2f941000502183b".into(),
                Err("map key is of type primitives"),
            ),
            (
                "map key",
                "a2a00502183c".into(),
                Err("map key is of type map"),
            ),
            (
                "tag key",
                "a2c1006178021837".into(),
                Err("map key is of type tag"),
            ),
            (
                "array key",
                "a28001021845".into(),
                Err("map key is of type array"),
            ),
            (
                "tag 0 content check",
                "a102c02a".into(),
                Err("tag number 0 must be followed by text string, got negative integer"),
            ),
            (
                "tag 1 content check",
                "a101c1c241aa".into(),
                Err("tag number 1 must be followed by integer or floating-point number, got tag"),
            ),
            (
                "reserved simple value",
                "a105f81002181d".into(),
                Err("invalid simple value 16"),
            ),
            (
                "mixed indefinite chunks",
                "a1015f4161616200ff".into(),
                Err("wrong element type UTF-8 text string for indefinite-length byte string"),
            ),
            (
                "nesting depth 33 rejected",
                format!("a205{}8002182a", "81".repeat(31)),
                Err("exceeded max nested level 32"),
            ),
            ("truncated value", "a2006162".into(), Err("unexpected EOF")),
            ("truncated map", "a3".into(), Err("unexpected EOF")),
            ("empty input", "".into(), Err("EOF")),
            ("bare break", "ff".into(), Err("break")),
        ];
        for (name, input, want) in &cases {
            let got = decode_meta(&hx(input));
            match want {
                Ok(r, root, at) => {
                    let m = got.unwrap_or_else(|e| panic!("{name}: unexpected error {e}"));
                    assert_eq!(m.ref_, *r, "{name}: ref");
                    assert_eq!(hex_string(&m.root), *root, "{name}: root");
                    assert_eq!(m.received_at, *at, "{name}: received_at");
                }
                Err(sub) => {
                    let e = match got {
                        Result::Ok(m) => panic!("{name}: expected error, got {m:?}"),
                        Result::Err(e) => e.to_string(),
                    };
                    assert!(e.contains(sub), "{name}: error {e:?} lacks {sub:?}");
                }
            }
        }
    }

    /// The three wrapping prefixes of `readMetaHeader` errors.
    #[test]
    fn read_meta_header_error_prefixes() {
        let err = read_meta_header(&mut Cursor::new(b"")).unwrap_err();
        assert!(err.to_string().starts_with("reading inbox meta length:"));
        // Length claims 10 bytes; only 4 present.
        let err = read_meta_header(&mut Cursor::new(hx("0000000aa3006060"))).unwrap_err();
        assert!(err.to_string().starts_with("reading inbox meta:"));
        // Well-framed garbage CBOR.
        let err = read_meta_header(&mut Cursor::new(hx("00000001ff"))).unwrap_err();
        assert!(err.to_string().starts_with("decoding inbox meta:"));
    }

    // -- inbox.go: Inbox ------------------------------------------------

    fn blob_object(data: &[u8]) -> (Key, Vec<u8>) {
        (Key::new(Type::Blob, data.len() as u64, data), data.to_vec())
    }

    fn pack_body(objs: &[(Key, Vec<u8>)]) -> Vec<u8> {
        let mut w = amberpack::Writer::new(Vec::new());
        for (k, d) in objs {
            w.add(*k, d).unwrap();
        }
        w.finish().unwrap()
    }

    fn new_test_store(dir: &Path) -> Arc<Store> {
        Arc::new(Store::open_with(dir.join("store"), Options::new().sync(false)).unwrap())
    }

    fn meta_for(root: Key) -> Meta {
        Meta {
            root: root.0.to_vec(),
            ..Default::default()
        }
    }

    /// Go: TestCommitProcessesIntoStore.
    #[test]
    fn commit_processes_into_store() {
        let td = tempfile::tempdir().unwrap();
        let store = new_test_store(td.path());
        let ib = Inbox::open(td.path().join("inbox"), Arc::clone(&store), 2, None).unwrap();

        let (root, data) = blob_object(b"hello inbox");
        let body = pack_body(&[(root, data)]);

        let mut meta = meta_for(root);
        meta.ref_ = "r".into();
        let (tmp, h, n) = ib.stage(&meta, &body[..]).unwrap();
        assert_eq!(n, body.len() as u64);
        // Only the body feeds the hash.
        assert_eq!(h, *blake3::hash(&body).as_bytes());
        assert!(ib.commit(&tmp, &h, root).unwrap());

        ib.wait_for(root);
        assert!(store.has(root).unwrap(), "object not stored after wait_for");
        ib.close();
    }

    /// Go: TestCommitIdempotent. Go's version leaves its single worker
    /// running and implicitly relies on the second Stage+Commit outrunning
    /// it (the first entry must still be in the directory for the duplicate
    /// check to see it) — a race the Rust threading loses in practice. The
    /// workers are parked first instead: Commit does not check `closed` (a
    /// ported Go quirk), so the idempotency path is identical, just
    /// deterministic.
    #[test]
    fn commit_idempotent() {
        let td = tempfile::tempdir().unwrap();
        let store = new_test_store(td.path());
        let ib = Inbox::open(td.path().join("inbox"), store, 1, None).unwrap();
        ib.close();

        let (root, data) = blob_object(b"dup payload");
        let body = pack_body(&[(root, data)]);

        let (tmp1, h1, _) = ib.stage(&meta_for(root), &body[..]).unwrap();
        assert!(ib.commit(&tmp1, &h1, root).unwrap(), "first commit");
        let (tmp2, h2, _) = ib.stage(&meta_for(root), &body[..]).unwrap();
        assert!(
            !ib.commit(&tmp2, &h2, root).unwrap(),
            "second commit of identical body should report added=false"
        );
        // The discarded duplicate's tmp file is gone.
        assert!(!tmp2.exists());
        ib.close();
    }

    /// Go: TestRecoveryResumesEntry.
    #[test]
    fn recovery_resumes_entry() {
        let td = tempfile::tempdir().unwrap();
        let store = new_test_store(td.path());
        let dir = td.path().join("inbox");
        fs::create_dir_all(&dir).unwrap();

        let (root, data) = blob_object(b"left behind by a crash");
        let body = pack_body(&[(root, data)]);

        let mut entry = Vec::new();
        write_meta_header(&mut entry, &meta_for(root)).unwrap();
        entry.extend_from_slice(&body);
        let name = format!("{}.pack", hex_string(blake3::hash(&body).as_bytes()));
        fs::write(dir.join(&name), &entry).unwrap();

        let ib = Inbox::open(&dir, Arc::clone(&store), 1, None).unwrap();
        ib.wait_for(root);
        assert!(store.has(root).unwrap(), "recovered entry not processed");
        assert!(!dir.join(&name).exists(), "processed entry not removed");
        ib.close();
    }

    /// Go: TestCorruptPackQuarantined (with a log callback bolted on to
    /// exercise the slog port).
    #[test]
    fn corrupt_pack_quarantined() {
        let td = tempfile::tempdir().unwrap();
        let store = new_test_store(td.path());
        let dir = td.path().join("inbox");
        let logs: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&logs);
        let log: LogFn = Box::new(move |line| unpoison(sink.lock()).push(line.to_string()));
        let ib = Inbox::open(&dir, store, 1, Some(log)).unwrap();

        let (root, _) = blob_object(b"x");
        let garbage = b"NOT-AN-AMBERPACK-STREAM";
        let (tmp, h, _) = ib.stage(&meta_for(root), &garbage[..]).unwrap();
        assert!(ib.commit(&tmp, &h, root).unwrap());

        ib.wait_for(root); // must release even though processing failed

        let name = format!("{}.pack", hex_string(&h));
        assert!(
            !dir.join(&name).exists(),
            "entry should have left the inbox dir"
        );
        assert!(
            dir.join("failed").join(&name).exists(),
            "entry should be quarantined under failed/"
        );
        ib.close();
        let logs = unpoison(logs.lock());
        assert!(
            logs.iter()
                .any(|l| l.starts_with("inbox: entry failed processing, quarantining name=")),
            "quarantine not logged: {logs:?}"
        );
    }

    /// Open sweeps everything out of tmp/ (files and empty directories).
    #[test]
    fn open_sweeps_tmp_dir() {
        let td = tempfile::tempdir().unwrap();
        let store = new_test_store(td.path());
        let dir = td.path().join("inbox");
        fs::create_dir_all(dir.join("tmp")).unwrap();
        fs::write(dir.join("tmp").join("stage-123"), b"partial").unwrap();
        fs::write(dir.join("tmp").join("stage-456"), b"").unwrap();
        fs::create_dir(dir.join("tmp").join("leftover")).unwrap();

        let ib = Inbox::open(&dir, store, 1, None).unwrap();
        let n = fs::read_dir(dir.join("tmp")).unwrap().count();
        assert_eq!(n, 0, "tmp/ not swept");
        ib.close();
    }

    /// Entries whose meta header cannot be read (truncated header, bad root
    /// key) are quarantined during recovery; non-entry files and directories
    /// are left alone.
    #[test]
    fn recovery_quarantines_unreadable_entries() {
        let td = tempfile::tempdir().unwrap();
        let store = new_test_store(td.path());
        let dir = td.path().join("inbox");
        fs::create_dir_all(&dir).unwrap();

        // Truncated header: only 2 of the 4 length bytes.
        fs::write(dir.join("truncated.pack"), [0u8, 0]).unwrap();
        // Valid CBOR meta but a 3-byte root that key::parse rejects.
        let mut short = Vec::new();
        write_meta_header(
            &mut short,
            &Meta {
                root: vec![1, 2, 3],
                ..Default::default()
            },
        )
        .unwrap();
        fs::write(dir.join("shortroot.pack"), &short).unwrap();
        // Not an entry: wrong suffix, and a directory with the suffix.
        fs::write(dir.join("notes.txt"), b"keep me").unwrap();
        fs::create_dir(dir.join("subdir.pack")).unwrap();

        let logs: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&logs);
        let ib = Inbox::open(
            &dir,
            store,
            1,
            Some(Box::new(move |l| unpoison(sink.lock()).push(l.into()))),
        )
        .unwrap();

        assert!(dir.join("failed").join("truncated.pack").exists());
        assert!(dir.join("failed").join("shortroot.pack").exists());
        assert!(!dir.join("truncated.pack").exists());
        assert!(!dir.join("shortroot.pack").exists());
        assert!(dir.join("notes.txt").exists());
        assert!(dir.join("subdir.pack").exists());
        {
            let logs = unpoison(logs.lock());
            let quarantined: Vec<_> = logs
                .iter()
                .filter(|l| {
                    l.starts_with("inbox: unreadable entry on recovery, quarantining name=")
                })
                .collect();
            assert_eq!(quarantined.len(), 2, "logs: {logs:?}");
        }
        // Nothing was enqueued: the barrier is empty for any root.
        ib.wait_for(blob_object(b"x").0);
        ib.close();
    }

    /// An entry with a readable header but a truncated body is enqueued on
    /// recovery and quarantined by the worker.
    #[test]
    fn recovery_enqueues_then_quarantines_truncated_body() {
        let td = tempfile::tempdir().unwrap();
        let store = new_test_store(td.path());
        let dir = td.path().join("inbox");
        fs::create_dir_all(&dir).unwrap();

        let (root, data) = blob_object(b"body will be cut short");
        let body = pack_body(&[(root, data)]);
        let mut entry = Vec::new();
        write_meta_header(&mut entry, &meta_for(root)).unwrap();
        entry.extend_from_slice(&body[..body.len() - 10]);
        fs::write(dir.join("cut.pack"), &entry).unwrap();

        let ib = Inbox::open(&dir, Arc::clone(&store), 1, None).unwrap();
        ib.wait_for(root); // must release despite the failure
        assert!(dir.join("failed").join("cut.pack").exists());
        assert!(!dir.join("cut.pack").exists());
        assert!(!store.has(root).unwrap());
        ib.close();
    }

    /// close() drains the queue before returning.
    #[test]
    fn close_drains_queued_work() {
        let td = tempfile::tempdir().unwrap();
        let store = new_test_store(td.path());
        let ib = Inbox::open(td.path().join("inbox"), Arc::clone(&store), 1, None).unwrap();

        let mut roots = Vec::new();
        for i in 0..3u8 {
            let (root, data) = blob_object(format!("drain me {i}").as_bytes());
            let body = pack_body(&[(root, data)]);
            let (tmp, h, _) = ib.stage(&meta_for(root), &body[..]).unwrap();
            assert!(ib.commit(&tmp, &h, root).unwrap());
            roots.push(root);
        }
        ib.close();
        for root in roots {
            assert!(store.has(root).unwrap(), "close returned before draining");
        }
    }

    /// The staged tmp file holds exactly [header][body]; discard removes it;
    /// a commit of a vanished tmp path fails.
    #[test]
    fn stage_layout_discard_and_bad_commit() {
        let td = tempfile::tempdir().unwrap();
        let store = new_test_store(td.path());
        let ib = Inbox::open(td.path().join("inbox"), store, 1, None).unwrap();

        let (root, data) = blob_object(b"layout");
        let body = pack_body(&[(root, data)]);
        let meta = meta_for(root);
        let (tmp, h, n) = ib.stage(&meta, &body[..]).unwrap();
        assert_eq!(n, body.len() as u64);

        let mut want = Vec::new();
        write_meta_header(&mut want, &meta).unwrap();
        want.extend_from_slice(&body);
        assert_eq!(fs::read(&tmp).unwrap(), want);

        ib.discard(&tmp);
        assert!(!tmp.exists());
        assert!(ib.commit(&tmp, &h, root).is_err(), "commit of removed tmp");
        ib.close();
    }
}
