//! Persists Amber-Store CAS objects in log-structured, append-only segment
//! (pack) files. Sealed segments are immutable, mmap'd whole, and self-indexed
//! by a footer (fanout index on the last key byte + binary fuse filter + fixed
//! trailer). An active segment is indexed in its owner's memory and, for
//! everybody else and for the next open, by a sidecar file beside it
//! (`sidecar.rs`). There is no global index. A directory may be open in any
//! number of stores, in any number of processes: a writer owns an active
//! segment of its own (`active.rs`), readers lock nothing and look at the
//! directory again when they miss (`view.rs`), and one lock file keeps
//! writers and a GC sweep apart (`gate.rs`). All format integers are
//! big-endian. Record framing lives in the [`crate::amberpack`] module. See
//! `architecture/packstore.md`.
//!
//! This is a semantic port of Go's `packstore` package; segment files are
//! interchangeable between the two implementations (see PORTING.md — zstd
//! frames differ, so segment *files* are not byte-identical run-to-run, but
//! each side reads the other's).

mod active;
mod barrier;
mod compact;
mod footer;
mod gate;
mod gc;
mod markset;
mod missing;
mod parallel;
mod prepare;
mod recover;
mod recover_sidecar;
mod repair;
mod sidecar;
mod verify;
mod view;

pub use compact::{CompactOpts, CompactStats, SegmentLiveness};
pub use gate::{Sweep, WriteSpan};
pub use gc::SegmentInfo;
pub use markset::MarkSet;
pub use parallel::{DEFAULT_BATCH_SIZE, WriteOpts, WriteStats};

use std::collections::{HashMap, HashSet};
use std::fs::{self, File};
use std::io;
use std::os::fd::AsRawFd;
use std::os::unix::fs::FileExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError, RwLock};
use std::time::SystemTime;

use crate::amberpack::{self, REC_HEADER_SIZE, decode_payload, encode_record};
use crate::key::Key;

use footer::SealedSegment;
use gate::Gate;
use prepare::prepare;
use recover::ActiveLoc;
use sidecar::{SIDECAR_SUFFIX, SidecarWriter};
use view::{
    DEFAULT_RACY_WINDOW, ForeignActive, Lookup, list_segments, publish_sealed,
    remove_orphan_sidecars, with_suffix,
};

/// First byte of the footer (Go: `tagSeal`).
pub(crate) const TAG_SEAL: u8 = 0xF0;

/// The 8-byte active/sealed segment file header (Go: `magicHeader`).
pub(crate) const MAGIC_HEADER: [u8; 8] = *b"AMBERSG\x01";

/// The 8-byte magic at the very end of a sealed segment (Go: `magicTrailer`).
pub(crate) const MAGIC_TRAILER: [u8; 8] = *b"AMBERSGF";

/// The default rotation threshold: the active segment is sealed once it
/// reaches this many bytes (Go: `DefaultSegmentSize`).
pub const DEFAULT_SEGMENT_SIZE: u64 = 2 << 30; // 2 GiB

const SEALED_SUFFIX: &str = ".seg";
const ACTIVE_SUFFIX: &str = ".seg.active";

/// One CAS object to store: its key and either its serialized bytes
/// (`data`) or, for an object that was encoded elsewhere, the complete
/// record as [`encode_record`] produced it (`record`). Exactly one of the two
/// is set: an object offered as a record carries an empty `data`. A record is
/// parsed (framing, CRC, canonical key, key equal to `key`) and appended
/// verbatim, so a caller that already holds encoded records, say a pack it
/// staged on disk, skips the compression round trip; with
/// [`WriteOpts::verify`] its payload is decoded and rehashed like `data` is
/// (Go: `packstore.Object`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Object {
    /// The object's 32-byte lookup key.
    pub key: Key,
    /// The object's serialized bytes; empty when `record` is set.
    pub data: Vec<u8>,
    /// The complete pre-encoded record, for an object that was encoded
    /// elsewhere.
    pub record: Option<Vec<u8>>,
}

impl From<crate::fstree::Object> for Object {
    fn from(o: crate::fstree::Object) -> Object {
        Object {
            key: o.key,
            data: o.bytes,
            record: None,
        }
    }
}

/// Errors from the packstore, mirroring the Go package's `errors.Is`
/// sentinels (`ErrNotFound`, `ErrClosed`, `ErrCorrupt`, `ErrVerify`). Match
/// classes with the `is_*` helpers where Go code would use `errors.Is`; they
/// see through [`Error::Context`] wrapping exactly like Go's `%w` chains.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// The key is not present in the store (Go: `ErrNotFound`).
    #[error("packstore: object not found")]
    NotFound,
    /// The store has been closed (Go: `ErrClosed`).
    #[error("packstore: store closed")]
    Closed,
    /// The id names no sealed segment — never sealed, or already removed
    /// (Go: `ErrUnknownSegment`).
    #[error("packstore: no such segment")]
    UnknownSegment,
    /// Structural corruption: bad record framing, bad footer, scrub findings
    /// (Go: `ErrCorrupt`, which aliases `amberpack.ErrCorrupt` — `msg` holds
    /// the complete diagnostic text, including that prefix where Go's
    /// wrapping produces it). `verify` marks scrub findings that Go wraps in
    /// *both* `ErrCorrupt` and `ErrVerify`.
    #[error("{msg}")]
    Corrupt {
        /// The complete diagnostic message.
        msg: String,
        /// Whether this corruption is also an object-verification failure.
        verify: bool,
    },
    /// An object's key does not match its payload (Go: `ErrVerify`, from
    /// `WriteParallel` with `verify` enabled). `msg` is the complete
    /// diagnostic text.
    #[error("{0}")]
    Verify(String),
    /// The write path was poisoned by an earlier fsync failure (Go: the
    /// sticky `packstore: write path failed: %w` error).
    #[error("packstore: write path failed: {0}")]
    Failed(String),
    /// [`Store::verify`] was canceled by its cancellation callback (Go:
    /// `ctx.Err()` from the caller's context).
    #[error("packstore: verify canceled")]
    Canceled,
    /// A wait for the store's gate was given up through the caller's
    /// cancellation callback (Go: the `ctx.Err()` that `BeginSweep` returns).
    #[error("packstore: canceled while waiting for the store's gate")]
    GateCanceled,
    /// A record-codec error surfaced unchanged (Go returns `amberpack` errors
    /// unwrapped; in practice the encode-side size limit).
    #[error(transparent)]
    Pack(amberpack::Error),
    /// An I/O error from the underlying files.
    #[error(transparent)]
    Io(#[from] io::Error),
    /// The filesystem refused space for a write that [`Options::preallocate`]
    /// asked it to reserve up front: `ENOSPC` or `EDQUOT`. Held apart from
    /// [`Error::Io`] because it is the one write failure that leaves the store
    /// intact — nothing was written, so the write path is not poisoned and the
    /// caller may retry after freeing space. No Go counterpart.
    #[error("pack allocation refused: {0}")]
    Capacity(#[source] io::Error),
    /// A diagnostic prefix wrapped around another error, preserving its
    /// classification (Go: `fmt.Errorf("...: %w", err)`).
    #[error("{msg}: {source}")]
    Context {
        /// The prefix text.
        msg: String,
        /// The wrapped error.
        source: Box<Error>,
    },
    /// An error the iterator handed to [`Store::write_batch`] /
    /// [`Store::write_parallel`] yielded, returned verbatim like Go returns
    /// the sequence's error.
    #[error(transparent)]
    Source(Box<dyn std::error::Error + Send + Sync>),
    /// Any other store-level failure (lock conflicts, mmap failures, filter
    /// construction).
    #[error("{0}")]
    Other(String),
}

impl Error {
    /// Go's `errors.Is(err, ErrNotFound)`.
    pub fn is_not_found(&self) -> bool {
        match self {
            Error::NotFound => true,
            Error::Context { source, .. } => source.is_not_found(),
            _ => false,
        }
    }

    /// Go's `errors.Is(err, ErrClosed)`.
    pub fn is_closed(&self) -> bool {
        match self {
            Error::Closed => true,
            Error::Context { source, .. } => source.is_closed(),
            _ => false,
        }
    }

    /// Go's `errors.Is(err, ErrUnknownSegment)`.
    pub fn is_unknown_segment(&self) -> bool {
        match self {
            Error::UnknownSegment => true,
            Error::Context { source, .. } => source.is_unknown_segment(),
            _ => false,
        }
    }

    /// Go's `errors.Is(err, ErrCorrupt)`.
    pub fn is_corrupt(&self) -> bool {
        match self {
            Error::Corrupt { .. } => true,
            Error::Pack(e) => e.is_corrupt(),
            Error::Context { source, .. } => source.is_corrupt(),
            _ => false,
        }
    }

    /// Go's `errors.Is(err, ErrVerify)`.
    pub fn is_verify(&self) -> bool {
        match self {
            Error::Verify(_) => true,
            Error::Corrupt { verify, .. } => *verify,
            Error::Context { source, .. } => source.is_verify(),
            _ => false,
        }
    }

    /// Whether the filesystem refused reserved space. No Go counterpart.
    pub fn is_capacity(&self) -> bool {
        match self {
            Error::Capacity(_) => true,
            Error::Context { source, .. } => source.is_capacity(),
            _ => false,
        }
    }
}

/// Classifies a failed reservation: a filesystem out of space or over quota is
/// [`Error::Capacity`], which leaves the store usable; anything else — a bad
/// descriptor, a filesystem without `fallocate`, a size past the filesystem's
/// maximum — is an ordinary [`Error::Io`]. No Go counterpart.
fn capacity_or_io(error: io::Error) -> Error {
    match error.raw_os_error() {
        Some(libc::ENOSPC) | Some(libc::EDQUOT) => Error::Capacity(error),
        _ => Error::Io(error),
    }
}

/// Builds a corruption error whose text matches Go's
/// `fmt.Errorf("%w: ...", ErrCorrupt)` (the sentinel is
/// `amberpack.ErrCorrupt`, so the prefix is amberpack's).
pub(crate) fn corrupt(detail: impl std::fmt::Display) -> Error {
    Error::Corrupt {
        msg: format!("amberpack: corrupt pack data: {detail}"),
        verify: false,
    }
}

/// Store configuration (Go: the `WithSegmentSize` / `WithSync` options).
#[derive(Debug, Clone, Copy)]
pub struct Options {
    preallocate: bool,
    segment_size: u64,
    sync: bool,
}

impl Default for Options {
    fn default() -> Options {
        Options {
            preallocate: false,
            segment_size: DEFAULT_SEGMENT_SIZE,
            sync: true,
        }
    }
}

impl Options {
    /// Returns the default configuration.
    pub fn new() -> Options {
        Options::default()
    }

    /// Reserves space with `fallocate` before every write to a segment, so a
    /// full or over-quota filesystem is refused up front, as
    /// [`Error::Capacity`], instead of failing mid-write. A caller that runs
    /// the store under a storage budget wants this; it costs one syscall per
    /// append. Linux only: a store opened with it elsewhere refuses every
    /// write. No Go counterpart.
    pub fn preallocate(mut self, enabled: bool) -> Options {
        self.preallocate = enabled;
        self
    }

    /// Sets the rotation threshold in bytes. A single oversized record may
    /// push one segment past it (Go: `WithSegmentSize`).
    pub fn segment_size(mut self, n: u64) -> Options {
        self.segment_size = n;
        self
    }

    /// Controls whether writes are fsynced for crash durability. Default is
    /// true; disabling it speeds bulk loads and tests (Go: `WithSync`).
    pub fn sync(mut self, b: bool) -> Options {
        self.sync = b;
        self
    }
}

/// The append-only segment this store owns and writes to. Other stores on
/// the directory own theirs (`active.rs`). `index` is written under the append
/// lock and read by lookups under the shared lock; the current size lives
/// with the writer in [`ActiveWriter`]. `f` holds the segment's flock (Go:
/// `activeSegment`).
struct ActiveSegment {
    id: u64,
    path: PathBuf,
    f: File,
    index: RwLock<HashMap<Key, ActiveLoc>>,
}

/// The write path's view of the active segment. Only the append lock holder
/// touches it (Go: `activeSegment.size`, "accessed only under appendMu").
struct ActiveWriter {
    seg: Arc<ActiveSegment>,
    size: u64,
    /// Mirrors the index on disk (`sidecar.rs`), so that the next open does
    /// not have to scan the data. `None` when it could not be written (Go:
    /// `activeSegment.sc`).
    sc: Option<SidecarWriter>,
}

impl ActiveWriter {
    fn sidecar_entry(&mut self, k: Key, loc: ActiveLoc) {
        if let Some(sc) = self.sc.as_mut() {
            sc.entry(k, loc);
        }
    }

    /// Records that everything written so far is durable.
    fn sidecar_synced(&mut self) {
        let size = self.size;
        if let Some(sc) = self.sc.as_mut() {
            sc.synced(size);
        }
    }
}

/// Write-path state, serialized by the append lock (Go: fields guarded by
/// `appendMu`, plus the directory handle that holds the flock).
struct AppendState {
    /// Holds the shared directory flock and serves directory fsyncs; dropped
    /// (and the lock released) on close.
    dir_f: Option<File>,
    /// The segment this store owns; `None` until its first write.
    active: Option<ActiveWriter>,
    /// A floor for new segment ids.
    next_id: u64,
}

/// Reader-visible state (Go: fields guarded by `mu`).
struct Shared {
    sealed: Vec<Arc<SealedSegment>>,    // ascending id
    active: Option<Arc<ActiveSegment>>, // the segment this store owns; None until its first write
    /// Active segments it does not own, indexed for reading.
    foreign: Vec<Arc<ForeignActive>>,
    /// Counts the changes this store itself made to the view, so that a
    /// refresh that raced one only adds (`view.rs`).
    struct_epoch: u64,
    /// The directory's modification time as of the view's last listing,
    /// taken at `listed_at`; together they let a lookup that finds nothing
    /// skip the next listing (`view_is_current`). `None`: not known, list.
    dir_mtime: Option<SystemTime>,
    listed_at: SystemTime,
    closed: bool,
    failed: Option<String>, // sticky write-path failure detail
}

/// A test hook.
type Hook = Box<dyn Fn() + Send + Sync>;

/// Test hooks; never set outside tests.
#[derive(Default)]
struct Hooks {
    /// Runs when a refresh has listed the directory, before it opens
    /// anything (Go: `afterList`).
    after_list: Mutex<Option<Hook>>,
    /// Runs when `compact` or `remove` took its victims out of the view,
    /// before it unlinks them (Go: `afterDetach`).
    after_detach: Mutex<Option<Hook>>,
}

/// An on-disk content-addressable store over segment files. It is safe for
/// concurrent use, by threads and — several stores on one directory — by
/// processes (`active.rs`, `view.rs`). Lock ordering: the append lock, then
/// the refresh lock, then the shared lock, never the reverse. The append lock
/// serializes the write path (append, fsync, seal, close); the refresh lock
/// serializes refreshes of the view; the shared lock guards
/// sealed/active/foreign/closed for readers (Go: `Store`).
pub struct Store {
    dir: PathBuf,
    cfg: Options,
    append: Mutex<AppendState>,
    shared: RwLock<Shared>,

    /// Write-barrier grey capture (Go: `capturing`/`greyMu`/`grey`; see
    /// barrier.rs). `capturing` is a lock-free fast path; the authoritative
    /// state is the `Option` under the mutex.
    capturing: AtomicBool,
    grey: Mutex<Option<HashSet<Key>>>,

    /// In-flight exported-write starts (Go: `writesMu`/`writes`; see gc.rs).
    writes: Mutex<gc::Writes>,

    /// Active-segment fsyncs issued, for tests (Go: `fsyncs`).
    fsyncs: AtomicU64,

    /// Serializes refreshes of the view. `refresh_seq` counts them, so that a
    /// lookup that waited for one does not repeat it; `refreshes` counts them
    /// for tests (Go: `refreshMu`/`refreshSeq`/`refreshes`).
    refresh_mu: Mutex<()>,
    refresh_seq: AtomicU64,
    refreshes: AtomicU64,
    /// See [`view::DEFAULT_RACY_WINDOW`]; tests shorten it.
    racy_window_nanos: AtomicU64,
    hooks: Hooks,

    /// `gc.lock`: writers against a sweep, across processes (`gate.rs`).
    gate: Gate,
}

impl std::fmt::Debug for Store {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Store")
            .field("dir", &self.dir)
            .finish_non_exhaustive()
    }
}

fn unpoison<T>(r: Result<T, PoisonError<T>>) -> T {
    r.unwrap_or_else(PoisonError::into_inner)
}

/// Reads the big-endian u32 at `off` in `b`.
pub(crate) fn be_u32(b: &[u8], off: usize) -> u32 {
    u32::from_be_bytes([b[off], b[off + 1], b[off + 2], b[off + 3]])
}

/// Parses a segment file's numeric id from its name (Go: `parseSegmentID`):
/// exactly 16 hex digits followed by `suffix`.
fn parse_segment_id(name: &[u8], suffix: &str) -> Result<u64, Error> {
    let bad = || {
        corrupt(format!(
            "bad segment file name {:?}",
            String::from_utf8_lossy(name)
        ))
    };
    let hex = name.strip_suffix(suffix.as_bytes()).ok_or_else(bad)?;
    if hex.len() != 16 {
        return Err(bad());
    }
    let hex = std::str::from_utf8(hex).map_err(|_| bad())?;
    if !hex.bytes().all(|c| c.is_ascii_hexdigit()) {
        return Err(bad());
    }
    u64::from_str_radix(hex, 16).map_err(|_| bad())
}

impl Store {
    /// Opens (creating if necessary) a store rooted at `dir` with the default
    /// [`Options`]. Any number of stores, in any number of processes, may
    /// have a directory open at once. Opening locks no segment and modifies
    /// none: sealed segments are mmap'd and validated, active ones indexed
    /// from their sidecars for reading. The store takes an active segment of
    /// its own at its first write (`active.rs`).
    ///
    /// The directory is flocked shared for the store's life. Releases from
    /// before stores could share a directory take that lock exclusively and
    /// assume they own the one active segment; this keeps them out, and keeps
    /// this store out while one of them is in (Go: `Open`).
    pub fn open(dir: impl AsRef<Path>) -> Result<Store, Error> {
        Store::open_with(dir, Options::default())
    }

    /// [`Store::open`] with explicit [`Options`].
    pub fn open_with(dir: impl AsRef<Path>, cfg: Options) -> Result<Store, Error> {
        let dir = dir.as_ref().to_path_buf();
        fs::create_dir_all(&dir)
            .map_err(|e| Error::Other(format!("packstore: creating {}: {e}", dir.display())))?;
        let dir_f = File::open(&dir)?;
        // SAFETY: plain flock(2) on a valid open fd; no memory is involved.
        let rc = unsafe { libc::flock(dir_f.as_raw_fd(), libc::LOCK_SH | libc::LOCK_NB) };
        if rc != 0 {
            let e = io::Error::last_os_error();
            return Err(Error::Other(format!(
                "packstore: {} is held by an older release, which needs the store to itself: {e}",
                dir.display()
            )));
        }
        let gate = Gate::open(&dir)?;
        if let Ok(ls) = list_segments(&dir) {
            remove_orphan_sidecars(&dir, &ls);
        }
        let store = Store {
            dir,
            cfg,
            append: Mutex::new(AppendState {
                dir_f: Some(dir_f),
                active: None,
                next_id: 1,
            }),
            shared: RwLock::new(Shared {
                sealed: Vec::new(),
                active: None,
                foreign: Vec::new(),
                struct_epoch: 0,
                dir_mtime: None,
                listed_at: SystemTime::UNIX_EPOCH,
                closed: false,
                failed: None,
            }),
            capturing: AtomicBool::new(false),
            grey: Mutex::new(None),
            writes: Mutex::new(gc::Writes::new()),
            fsyncs: AtomicU64::new(0),
            refresh_mu: Mutex::new(()),
            refresh_seq: AtomicU64::new(0),
            refreshes: AtomicU64::new(0),
            racy_window_nanos: AtomicU64::new(DEFAULT_RACY_WINDOW.as_nanos() as u64),
            hooks: Hooks::default(),
            gate,
        };
        {
            let held = unpoison(store.refresh_mu.lock());
            store.refresh_locked(&held, true)?;
        }
        Ok(store)
    }

    fn append_lock(&self) -> MutexGuard<'_, AppendState> {
        unpoison(self.append.lock())
    }

    /// Writes one encoded record to the active segment (creating it if
    /// needed), publishes it in the active index, optionally fsyncs, and
    /// seals the segment if it reached the rotation threshold (Go: `append`).
    fn append(&self, k: Key, rec: &[u8], sync_now: bool) -> Result<(), Error> {
        let mut ap = self.append_lock();
        self.append_locked(&mut ap, k, rec, sync_now)
    }

    /// [`Store::append`]'s body. The caller must hold the append lock (Go:
    /// `appendLocked`; [`Store::compact`]'s appender calls it directly since
    /// it already holds the lock).
    fn append_locked(
        &self,
        ap: &mut AppendState,
        k: Key,
        rec: &[u8],
        sync_now: bool,
    ) -> Result<(), Error> {
        {
            let sh = unpoison(self.shared.read());
            if sh.closed {
                return Err(Error::Closed);
            }
            if let Some(msg) = &sh.failed {
                return Err(Error::Failed(msg.clone()));
            }
        }
        self.ensure_active(ap)?;
        let Some(aw) = ap.active.as_mut() else {
            return Err(Error::Closed); // unreachable: ensure_active succeeded
        };
        if unpoison(aw.seg.index.read()).contains_key(&k) {
            return Ok(()); // lost a Put race for this key; the record is already appended
        }
        let off = aw.size;
        if let Err(error) = self.reserve_file(&aw.seg.f, off, rec.len() as u64) {
            aw.seg.f.set_len(aw.size)?;
            return Err(error);
        }
        aw.seg.f.write_all_at(rec, off)?;
        let loc = ActiveLoc {
            off,
            flags: rec[33],
            ulen: be_u32(rec, 34),
            slen: be_u32(rec, 38),
        };
        unpoison(aw.seg.index.write()).insert(k, loc);
        aw.size = off + rec.len() as u64;
        aw.sidecar_entry(k, loc); // only now: an entry never precedes its record

        if sync_now && self.cfg.sync {
            let res = aw.seg.f.sync_all();
            if let Err(e) = res {
                self.set_failed(&e);
                return Err(e.into());
            }
            aw.sidecar_synced();
        }
        if ap
            .active
            .as_ref()
            .is_some_and(|a| a.size >= self.cfg.segment_size)
        {
            // A mid-seal failure can leave a renamed-but-unpublished segment;
            // reads stay correct (the fd is still open), but accepting
            // further writes could append past a footer. Poison the write
            // path; reopen recovers cleanly. A refused reservation wrote
            // nothing, so it leaves no such tail and must not poison.
            if let Err(e) = self.seal_active(ap) {
                if !e.is_capacity() {
                    self.set_failed(&e);
                }
                return Err(e);
            }
        }
        Ok(())
    }

    /// Fsyncs the active segment, if syncing is enabled and one exists (Go:
    /// `syncActive`).
    fn sync_active(&self) -> Result<(), Error> {
        let mut ap = self.append_lock();
        {
            let sh = unpoison(self.shared.read());
            if sh.closed {
                return Err(Error::Closed);
            }
            if let Some(msg) = &sh.failed {
                return Err(Error::Failed(msg.clone()));
            }
        }
        if !self.cfg.sync {
            return Ok(());
        }
        let Some(aw) = ap.active.as_mut() else {
            return Ok(());
        };
        if let Err(e) = aw.seg.f.sync_all() {
            self.set_failed(&e);
            return Err(e.into());
        }
        aw.sidecar_synced();
        self.fsyncs
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        Ok(())
    }

    /// Poisons the write path after an fsync failure: a failed fsync may have
    /// dropped dirty pages, so later appends could be acknowledged while
    /// sitting behind a garbage hole that tail-scan recovery would truncate.
    /// Reads stay available; only new acknowledgments stop. Called under the
    /// append lock (Go: `setFailed`).
    fn set_failed(&self, err: &dyn std::fmt::Display) {
        let mut sh = unpoison(self.shared.write());
        if sh.failed.is_none() {
            sh.failed = Some(err.to_string());
        }
    }

    /// Reserves `len` bytes at `offset` of `file` when
    /// [`Options::preallocate`] is set, so a write that the filesystem cannot
    /// satisfy is refused before it starts. `FALLOC_FL_KEEP_SIZE` leaves the
    /// file length alone: the segment's length is its data, and recovery scans
    /// to it. `ENOSPC` and `EDQUOT` come back as [`Error::Capacity`], every
    /// other errno as [`Error::Io`]. No Go counterpart.
    fn reserve_file(&self, file: &File, offset: u64, len: u64) -> Result<(), Error> {
        if !self.cfg.preallocate || len == 0 {
            return Ok(());
        }
        #[cfg(target_os = "linux")]
        {
            let offset = i64::try_from(offset)
                .map_err(|_| Error::Other("pack offset exceeds file range".into()))?;
            let len = i64::try_from(len)
                .map_err(|_| Error::Other("pack allocation exceeds file range".into()))?;
            let result = unsafe {
                libc::fallocate(file.as_raw_fd(), libc::FALLOC_FL_KEEP_SIZE, offset, len)
            };
            if result == 0 {
                return Ok(());
            }
            Err(capacity_or_io(io::Error::last_os_error()))
        }
        #[cfg(not(target_os = "linux"))]
        Err(Error::Other("pack preallocation requires Linux".into()))
    }

    /// Seals the active segment: build the footer from the in-RAM index (no
    /// body re-read), append it, fsync, rename to `.seg`, fsync the
    /// directory, and swap in the mmap'd sealed segment. Called under the
    /// append lock (Go: `sealActiveLocked`).
    fn seal_active(&self, ap: &mut AppendState) -> Result<(), Error> {
        let Some(aw) = ap.active.as_ref() else {
            return Ok(());
        };
        let entries: Vec<footer::IndexEntry> = unpoison(aw.seg.index.read())
            .iter()
            .map(|(k, loc)| footer::IndexEntry {
                k: *k,
                off: loc.off,
                slen: loc.slen,
            })
            .collect();
        if entries.is_empty() {
            return Ok(());
        }
        let ftr = footer::build_footer(aw.size, &entries)?;
        // The footer is located from EOF, so drop anything a failed write
        // left past aw.size.
        aw.seg.f.set_len(aw.size)?;
        if let Err(error) = self.reserve_file(&aw.seg.f, aw.size, ftr.len() as u64) {
            aw.seg.f.set_len(aw.size)?;
            return Err(error);
        }
        aw.seg.f.write_all_at(&ftr, aw.size)?;
        aw.seg.f.sync_all()?;
        let sealed_path = view::sealed_path_of(&aw.seg.path);
        fs::rename(&aw.seg.path, &sealed_path)?;
        let Some(dir_f) = ap.dir_f.as_ref() else {
            return Err(Error::Closed);
        };
        dir_f.sync_all()?;
        // The footer indexes the segment from here on. A crash before the
        // removal leaves an orphan that the next open deletes.
        let _ = fs::remove_file(with_suffix(&aw.seg.path, SIDECAR_SUFFIX));
        let seg = SealedSegment::open(&sealed_path, aw.seg.id)?;
        {
            let mut sh = unpoison(self.shared.write());
            publish_sealed(&mut sh, Arc::new(seg));
            sh.active = None;
        }
        // Drop the writer's handle only after the swap: readers that resolved
        // a location before it route to the fd their own Arc keeps alive, and
        // post-swap readers route to the sealed mmap. (Go closes the fd here
        // and can surface a close error; Rust closes it when the last Arc
        // drops, which cannot report one — see port-notes.)
        ap.active = None;
        Ok(())
    }

    /// Stores every object the iterator yields, fsyncing once at the end
    /// (when syncing is enabled): on return, all yielded objects are durable.
    /// It is NOT atomic — a crash or iterator error can leave a valid prefix
    /// stored. In a content-addressed store that prefix is harmless:
    /// identical re-pushed content deduplicates. Objects repeated within the
    /// batch, or already present, are written once. When `write_batch`
    /// returns an error after appending part of the batch, it best-effort
    /// fsyncs that prefix first, so visible records never stay non-durable
    /// (Go: `WriteBatch`).
    pub fn write_batch<I, E>(&self, seq: I) -> Result<(), Error>
    where
        I: IntoIterator<Item = Result<Object, E>>,
        E: std::error::Error + Send + Sync + 'static,
    {
        let _write_token = self.begin_write_token()?;
        let mut seen: HashSet<Key> = HashSet::new();
        let mut appended = false;
        let fail = |appended: bool, err: Error| -> Error {
            if appended {
                let _ = self.sync_active(); // best-effort; an fsync failure poisons the store
            }
            err
        };
        for item in seq {
            let obj = match item {
                Ok(o) => o,
                Err(e) => return Err(fail(appended, Error::Source(Box::new(e)))),
            };
            if !seen.insert(obj.key) {
                continue;
            }
            // Observe before the dedup check: a barrier capture must grey
            // dedup hits too (a hit in a condemned pack is otherwise lost).
            self.observe(obj.key);
            let has = match self.has_durably(obj.key) {
                Ok(h) => h,
                Err(e) => {
                    return Err(fail(
                        appended,
                        Error::Context {
                            msg: format!("exists ({})", obj.key),
                            source: Box::new(e),
                        },
                    ));
                }
            };
            if has {
                continue;
            }
            let key = obj.key;
            let rec = match prepare(obj, false) {
                Ok((r, _)) => r,
                Err(e) => return Err(fail(appended, e)),
            };
            if let Err(e) = self.append(key, &rec, false) {
                return Err(fail(appended, e));
            }
            appended = true;
        }
        self.sync_active()
    }

    /// Stores a single object under `k`, deduplicating against existing
    /// content. A dedup hit returns success without fsyncing; if the matching
    /// record was appended by a still-running batch, its durability rides on
    /// that batch's commit (Go: `Put`).
    pub fn put(&self, k: Key, data: &[u8]) -> Result<(), Error> {
        let _write_token = self.begin_write_token()?;
        {
            let sh = unpoison(self.shared.read());
            if let Some(msg) = &sh.failed {
                return Err(Error::Failed(msg.clone()));
            }
        }
        // Observe before the dedup check: a barrier capture must grey dedup
        // hits too (a hit in a condemned pack is otherwise lost).
        self.observe(k);
        if self.has_durably(k)? {
            return Ok(());
        }
        let rec = encode_record(k, data).map_err(Error::Pack)?;
        self.append(k, &rec, true)
    }

    /// Returns the bytes stored under `k`, or [`Error::NotFound`] if `k` is
    /// absent. The returned buffer is caller-owned (Go: `Get`).
    pub fn get(&self, k: Key) -> Result<Vec<u8>, Error> {
        self.lookup(k, || self.get_once(k))
    }

    /// Finds `k` in the active segment this store owns or in one it only
    /// reads. The caller holds the shared lock (Go: `activeLookupLocked`).
    fn active_lookup(sh: &Shared, k: Key) -> Option<ActiveHit<'_>> {
        if let Some(a) = &sh.active
            && let Some(loc) = unpoison(a.index.read()).get(&k).copied()
        {
            return Some(ActiveHit::Own(a, loc));
        }
        for fa in &sh.foreign {
            if let Some(loc) = unpoison(fa.state.read()).scan.index.get(&k).copied() {
                return Some(ActiveHit::Foreign(fa, loc));
            }
        }
        None
    }

    /// Runs `find` and, if it found nothing, once more after a fresh look at
    /// the directory: another store may have written `k` since this one last
    /// looked (Go: `lookup`).
    fn lookup<T>(&self, k: Key, find: impl Fn() -> Result<Lookup<T>, Error>) -> Result<T, Error> {
        let mut found = find()?;
        if !matches!(found, Lookup::Hit(_)) {
            let stale = matches!(found, Lookup::Stale);
            if self.refresh_after_miss(stale)? {
                found = find()?;
            }
        }
        match found {
            Lookup::Hit(v) => Ok(v),
            Lookup::Miss => Err(Error::NotFound),
            Lookup::Stale => Err(corrupt(format!(
                "{k}: another writer's active segment does not hold the record its index names"
            ))),
        }
    }

    fn get_once(&self, k: Key) -> Result<Lookup<Vec<u8>>, Error> {
        let sh = unpoison(self.shared.read());
        if sh.closed {
            return Err(Error::Closed);
        }
        if let Some(hit) = Store::active_lookup(&sh, k) {
            let (loc, stored) = match hit {
                ActiveHit::Foreign(fa, loc) => match fa.read(k, loc)? {
                    Lookup::Hit(stored) => (loc, stored),
                    Lookup::Miss => return Ok(Lookup::Miss),
                    Lookup::Stale => return Ok(Lookup::Stale),
                },
                ActiveHit::Own(a, loc) => {
                    let mut stored = vec![0u8; loc.slen as usize];
                    a.f.read_exact_at(&mut stored, loc.off + REC_HEADER_SIZE as u64)?;
                    (loc, stored)
                }
            };
            return decode_payload(loc.flags, loc.ulen, &stored)
                .map(Lookup::Hit)
                .map_err(|e| Error::Corrupt {
                    msg: e.to_string(),
                    verify: false,
                });
        }
        for seg in sh.sealed.iter().rev() {
            // A corrupt segment fails the read loudly rather than falling
            // back to older copies: masking corruption would hide real damage
            // from scrub.
            if let Some(data) = seg.get(k)? {
                return Ok(Lookup::Hit(data));
            }
        }
        Ok(Lookup::Miss)
    }

    /// Returns a caller-owned copy of the full on-disk record stored under
    /// `k` — its 46-byte header plus the stored (still-compressed) payload,
    /// exactly as written by [`encode_record`] — or [`Error::NotFound`] if
    /// `k` is absent. This is the zero-copy push path: the record is
    /// wire-format-identical, so a caller can hand it to
    /// `amberpack::Writer::add_record` without decompressing and re-encoding.
    /// Like [`Store::get`], it does not CRC-check; the receiving reader
    /// validates framing and CRC (Go: `GetRecord`).
    pub fn get_record(&self, k: Key) -> Result<Vec<u8>, Error> {
        self.lookup(k, || self.get_record_once(k))
    }

    fn get_record_once(&self, k: Key) -> Result<Lookup<Vec<u8>>, Error> {
        let sh = unpoison(self.shared.read());
        if sh.closed {
            return Err(Error::Closed);
        }
        if let Some(hit) = Store::active_lookup(&sh, k) {
            return match hit {
                ActiveHit::Foreign(fa, loc) => fa.read_record(k, loc),
                ActiveHit::Own(a, loc) => {
                    let mut rec = vec![0u8; REC_HEADER_SIZE + loc.slen as usize];
                    a.f.read_exact_at(&mut rec, loc.off)?;
                    Ok(Lookup::Hit(rec))
                }
            };
        }
        for seg in sh.sealed.iter().rev() {
            if let Some(rec) = seg.get_record(k)? {
                return Ok(Lookup::Hit(rec));
            }
        }
        Ok(Lookup::Miss)
    }

    /// Returns the stored (post-compression) payload length of the object
    /// under `k`, or `None` if absent, reading only the index — no payload
    /// read. It sizes objects for byte-balanced push batching against the
    /// bytes that actually travel (Go: `StoredSize`).
    pub fn stored_size(&self, k: Key) -> Result<Option<u64>, Error> {
        let mut size = self.stored_size_once(k)?;
        if size.is_none() && self.refresh_after_miss(false)? {
            size = self.stored_size_once(k)?;
        }
        Ok(size)
    }

    fn stored_size_once(&self, k: Key) -> Result<Option<u64>, Error> {
        let sh = unpoison(self.shared.read());
        if sh.closed {
            return Err(Error::Closed);
        }
        if let Some(hit) = Store::active_lookup(&sh, k) {
            return Ok(Some(u64::from(hit.loc().slen)));
        }
        for seg in sh.sealed.iter().rev() {
            if let Some(slen) = seg.stored_size(k) {
                return Ok(Some(u64::from(slen)));
            }
        }
        Ok(None)
    }

    /// Returns the segment id and record offset where `k` lives, for ordering
    /// reads by physical layout. Caller holds the shared lock (Go:
    /// `locateLocked`).
    fn locate_in(sh: &Shared, k: Key) -> Option<(u64, u64)> {
        match Store::active_lookup(sh, k) {
            Some(ActiveHit::Own(a, loc)) => return Some((a.id, loc.off)),
            Some(ActiveHit::Foreign(fa, loc)) => return Some((fa.id, loc.off)),
            None => {}
        }
        for seg in sh.sealed.iter().rev() {
            if let Some(off) = seg.locate(k) {
                return Some((seg.id, off));
            }
        }
        None
    }

    /// Reorders `keys` in place to follow the store's on-disk layout —
    /// grouped by segment, ascending offset within a segment — so reading
    /// them in order is a near-sequential sweep per segment rather than
    /// scattered random access. Absent keys sort last (their reads surface
    /// [`Error::NotFound`] later). It is a no-op on a closed store (Go:
    /// `SortByLocation`).
    pub fn sort_by_location(&self, keys: &mut [Key]) {
        struct Located {
            k: Key,
            seg: u64,
            off: u64,
            ok: bool,
        }
        let mut items: Vec<Located> = {
            let sh = unpoison(self.shared.read());
            if sh.closed {
                return;
            }
            keys.iter()
                .map(|&k| match Store::locate_in(&sh, k) {
                    Some((seg, off)) => Located {
                        k,
                        seg,
                        off,
                        ok: true,
                    },
                    None => Located {
                        k,
                        seg: 0,
                        off: 0,
                        ok: false,
                    },
                })
                .collect()
        };
        items.sort_by(|a, b| {
            // Present keys before absent ones, then (segment, offset).
            (!a.ok, a.seg, a.off).cmp(&(!b.ok, b.seg, b.off))
        });
        for (dst, item) in keys.iter_mut().zip(&items) {
            *dst = item.k;
        }
    }

    /// Reports whether an object is stored under `k` (Go: `Has`).
    pub fn has(&self, k: Key) -> Result<bool, Error> {
        let mut has = self.has_local(k)?;
        if !has && self.refresh_after_miss(false)? {
            has = self.has_local(k)?;
        }
        Ok(has)
    }

    /// The write path's duplicate check: whether the view holds a copy of `k`
    /// that a write may rely on instead of making one. It does not look at
    /// the directory again. A duplicate it fails to see — written by another
    /// store a moment ago — costs a redundant record, which compaction folds;
    /// listing the directory for every new object would cost every ingest
    /// dearly (Go: `hasDurably`).
    pub(crate) fn has_durably(&self, k: Key) -> Result<bool, Error> {
        let sh = unpoison(self.shared.read());
        if sh.closed {
            return Err(Error::Closed);
        }
        Ok(Store::reliable_active(&sh, self.cfg.sync, k)
            || sh.sealed.iter().rev().any(|seg| seg.has(k)))
    }

    /// Reports whether an active segment holds a copy of `k` that a write, or
    /// a compaction, may rely on instead of making its own. A record in
    /// another store's segment counts only as far as its owner has synced it,
    /// when this store syncs: this store's fsync covers its own segment
    /// alone, and acknowledging a write against bytes somebody else has yet
    /// to sync would promise what nobody has delivered. The price is a second
    /// copy now and then. The caller holds the shared lock (Go:
    /// `reliableActiveLocked`).
    fn reliable_active(sh: &Shared, sync: bool, k: Key) -> bool {
        if let Some(a) = &sh.active
            && unpoison(a.index.read()).contains_key(&k)
        {
            return true; // this store's own fsync covers it
        }
        sh.foreign.iter().any(|fa| {
            let st = unpoison(fa.state.read());
            st.scan.index.get(&k).is_some_and(|loc| {
                !sync
                    || (loc.off as i64)
                        .wrapping_add(REC_HEADER_SIZE as i64)
                        .wrapping_add(i64::from(loc.slen))
                        <= st.scan.at.durable
            })
        })
    }

    /// [`Store::has`] without the second look: what this store's view holds,
    /// synced or not. Reads ask it; writes ask [`Store::has_durably`] (Go:
    /// `hasLocal`).
    pub(crate) fn has_local(&self, k: Key) -> Result<bool, Error> {
        let sh = unpoison(self.shared.read());
        if sh.closed {
            return Err(Error::Closed);
        }
        Ok(Store::active_lookup(&sh, k).is_some() || sh.sealed.iter().rev().any(|seg| seg.has(k)))
    }

    /// Deletes every object: every segment, sealed or active, is detached and
    /// its file removed, leaving an empty, still-open store (the store-wipe
    /// operation). It refuses, deleting nothing, while another store owns an
    /// active segment: that writer would go on appending to a file that is
    /// gone. Readers are drained via the write lock before segments are
    /// detached; an in-flight [`Store::verify`] keeps its snapshot's mappings
    /// alive independently (Go: `Wipe`).
    pub fn wipe(&self) -> Result<(), Error> {
        let _sweep = self.sweep_gate()?; // other stores' writers wait, and look again afterwards
        let mut ap = self.append_lock();
        // Held throughout: the view must not grow behind the locks taken below.
        let held = unpoison(self.refresh_mu.lock());
        self.refresh_locked(&held, false)?;
        let foreign: Vec<Arc<ForeignActive>> = unpoison(self.shared.read()).foreign.clone();
        let _locked = self.lock_foreign(&foreign)?;

        let (active, sealed) = {
            let mut sh = unpoison(self.shared.write());
            if sh.closed {
                return Err(Error::Closed);
            }
            // A sticky write-path failure poisons the data the fsync may have
            // torn — data the wipe is about to destroy. The reset clears it:
            // the reopened-empty store must accept writes again.
            sh.failed = None;
            sh.foreign.clear();
            sh.struct_epoch += 1;
            (sh.active.take(), std::mem::take(&mut sh.sealed))
        };
        ap.active = None;

        let mut first_err: Option<Error> = None;
        let mut note = |res: io::Result<()>| {
            if let Err(e) = res
                && e.kind() != io::ErrorKind::NotFound
                && first_err.is_none()
            {
                first_err = Some(e.into());
            }
        };
        if let Some(a) = active {
            note(fs::remove_file(&a.path));
            note(fs::remove_file(with_suffix(&a.path, SIDECAR_SUFFIX)));
            // The fd closes when in-flight readers drop their handles.
        }
        for fa in &foreign {
            note(fs::remove_file(&fa.path));
            note(fs::remove_file(with_suffix(&fa.path, SIDECAR_SUFFIX)));
        }
        for seg in sealed {
            note(fs::remove_file(&seg.path));
        }
        if self.cfg.sync
            && let Some(dir_f) = ap.dir_f.as_ref()
        {
            note(dir_f.sync_all());
        }
        first_err.map_or(Ok(()), Err)
    }

    /// Fsyncs and lets go of the active segment this store owns (without
    /// sealing it, so that the next writer goes on filling it), detaches all
    /// sealed segments, and releases the segment and directory locks.
    /// Idempotent (Go: `Close`; an in-flight [`Store::verify`] keeps its
    /// snapshot alive, so unlike Go there is nothing to wait for — see
    /// port-notes).
    pub fn close(&self) -> Result<(), Error> {
        let mut ap = self.append_lock();
        let mut sh = unpoison(self.shared.write());
        if sh.closed {
            return Ok(());
        }
        sh.closed = true;
        let mut first_err: Option<Error> = None;
        if let Some(mut aw) = ap.active.take() {
            // Synced whatever the sync option says, so that a store closed
            // cleanly always reopens from its sidecar alone.
            match aw.seg.f.sync_all() {
                Ok(()) => aw.sidecar_synced(),
                Err(e) => first_err = Some(e.into()),
            }
            active::unlock(&aw.seg.f); // releases the segment
        }
        sh.active = None;
        sh.sealed.clear();
        sh.foreign.clear();
        drop(sh);
        self.gate.close();
        ap.dir_f = None; // releases the directory flock
        first_err.map_or(Ok(()), Err)
    }
}

/// Where [`Store::active_lookup`] found a key.
enum ActiveHit<'a> {
    Own(&'a ActiveSegment, ActiveLoc),
    Foreign(&'a ForeignActive, ActiveLoc),
}

impl ActiveHit<'_> {
    fn loc(&self) -> ActiveLoc {
        match self {
            ActiveHit::Own(_, loc) | ActiveHit::Foreign(_, loc) => *loc,
        }
    }
}

impl Drop for Store {
    fn drop(&mut self) {
        let _ = self.close();
    }
}

#[cfg(test)]
pub(crate) mod testutil;

#[cfg(test)]
mod gc_tests;

#[cfg(test)]
mod store_tests;

#[cfg(test)]
mod record_tests;

#[cfg(test)]
mod repair_tests;

#[cfg(test)]
mod sidecar_tests;

#[cfg(test)]
mod multi_tests;

#[cfg(test)]
mod gate_tests;

#[cfg(test)]
mod prealloc_tests;
