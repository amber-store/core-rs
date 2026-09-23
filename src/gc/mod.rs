//! The mark-and-sweep collector of `architecture/mark-sweep-gc.md`, a port of
//! Mic92's bitmap GC (draganm/amber-store#9): a cycle marks every key
//! reachable from the references' roots into a [`crate::packstore::MarkSet`]
//! — one bit per sealed record, slotted by the packs' own footer indexes —
//! and sweeps by rewriting the packs whose dead ratio crosses the line
//! ([`crate::packstore::Store::compact`]). Nothing is persisted between
//! cycles: no closure files, no union, no refcounts. A write barrier
//! ([`crate::packstore::Store::begin_barrier`]) keeps ingests running while
//! the mark walks; reference publication and the sweep serialize on the
//! collector's reference lock. Wire protocol and pack formats are unchanged.
//!
//! This is a semantic port of Go's `gc` package (`gc.go`, `collector.go`,
//! `cycle.go`, `status.go`). Go's context/goroutine plumbing maps to a
//! per-cycle cancel flag plus a plain thread for the background loop; see
//! `port-notes/gc.md`.

mod collector;
mod cycle;
mod status;

#[cfg(test)]
mod multi_tests;
#[cfg(test)]
mod tests;

pub use collector::{Collector, PreparedRef, Span, WriteGate};
pub use cycle::CycleStats;
pub use status::{PackStatus, Status};

use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard, PoisonError, RwLock, RwLockReadGuard, RwLockWriteGuard};
use std::time::{Duration, Instant};

use crate::fstree;
use crate::key::Key;
use crate::packstore;
use crate::refstore;

/// The minimum age of a sealed pack before it can be reaped (Go:
/// `DefaultGrace`).
pub const DEFAULT_GRACE: Duration = Duration::from_secs(60 * 60);

/// The selection line: an eligible pack with at least this fraction of
/// garbage is reaped (Go: `DefaultGarbage`).
pub const DEFAULT_GARBAGE: f64 = 0.5;

/// The selection line under free-space pressure (Go: `minFreeGarbage`).
const MIN_FREE_GARBAGE: f64 = 0.1;

/// Configures a [`Collector`]. The zero value ([`Options::default`]) means:
/// 1 h grace, 0.5 garbage line, min-free at 5 % of the filesystem, unlimited
/// copy rate, no background cycles, all-cores walk parallelism (Go:
/// `Options`).
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Options {
    /// Pack eligibility age (Go: `Grace`; zero selects [`DEFAULT_GRACE`] —
    /// Go's negative values are unrepresentable in a [`Duration`]).
    pub grace: Duration,
    /// Reap packs with at least this fraction of garbage (Go: `Garbage`;
    /// `<= 0` selects [`DEFAULT_GARBAGE`]).
    pub garbage: f64,
    /// Free-space floor in bytes; 0 = 5 % of the filesystem (Go: `MinFree`).
    pub min_free: u64,
    /// Copier bandwidth cap in bytes/s; `<= 0` = unlimited (Go: `Rate`).
    pub rate: i64,
    /// Time between background cycles; zero = none (Go: `Interval`).
    pub interval: Duration,
    /// [`Collector::prepare_ref`] walk parallelism; 0 = all cores (Go:
    /// `Jobs`, defaulted to `GOMAXPROCS`).
    pub jobs: usize,
}

impl Options {
    /// Fills in the defaulted fields; `min_free`, `rate` and `interval` are
    /// not defaulted — zero is meaningful for them (Go: `withDefaults`).
    fn with_defaults(mut self) -> Options {
        if self.grace == Duration::ZERO {
            self.grace = DEFAULT_GRACE;
        }
        if self.garbage <= 0.0 {
            self.garbage = DEFAULT_GARBAGE;
        }
        if self.jobs == 0 {
            self.jobs = std::thread::available_parallelism().map_or(1, |n| n.get());
        }
        self
    }
}

/// Errors from the collector. One variant per Go wrap site or sentinel;
/// variants Go returns unwrapped display transparently as the inner error.
/// Match classes with the `is_*` helpers where Go code would use
/// `errors.Is`.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// An overlapping [`Collector::run`]; cycles never overlap and are never
    /// queued (Go: `ErrCycleRunning`).
    #[error("gc: a cycle is already running")]
    CycleRunning,
    /// The cycle was canceled by [`Collector::close`] or [`Collector::wipe`]
    /// (Go: the per-cycle context's `context.Canceled`).
    #[error("gc: cycle canceled")]
    Canceled,
    /// Creating the closures directory failed (Go: `"gc: creating %s: %w"`).
    #[error("gc: creating {}: {source}", dir.display())]
    Creating {
        /// The directory that could not be created.
        dir: PathBuf,
        /// The underlying failure.
        source: std::io::Error,
    },
    /// Listing the closures directory at open failed (Go returns the
    /// `os.ReadDir` error unwrapped).
    #[error(transparent)]
    Io(std::io::Error),
    /// Removing leftover closure state at open failed; open fails, it does
    /// not warn (Go: `"gc: sweeping stale closure state: %w"`).
    #[error("gc: sweeping stale closure state: {0}")]
    Sweep(std::io::Error),
    /// The completeness walk under [`Collector::prepare_ref`] failed — the
    /// caller's 404 (Go: `"gc: walking root %s: %w"`).
    #[error("gc: walking root {root}: {source}")]
    Walk {
        /// The root whose tree was being walked.
        root: Key,
        /// The walk failure; a missing object surfaces here as
        /// [`fstree::WalkError::Missing`]. Boxed to keep the enum small.
        source: Box<fstree::WalkError<packstore::Error>>,
    },
    /// The mark reached a key absent from the object store; the mark aborts
    /// loudly rather than sweep over an inconsistency (Go: `"gc: mark:
    /// object %s missing from store"`).
    #[error("gc: mark: object {key} missing from store")]
    MissingFromStore {
        /// The absent object's key.
        key: Key,
    },
    /// A reference record failed to decode or parse, or its tree walk failed
    /// in [`Collector::why`] (Go: `"gc: reference %q: %w"`).
    #[error("gc: reference {name:?}: {source}")]
    Reference {
        /// The reference's name.
        name: String,
        /// The decode/parse/walk failure.
        source: Box<dyn std::error::Error + Send + Sync>,
    },
    /// Opening a write span failed: the store is closing (Go: `"gc: %w"` in
    /// `BeginSpan`).
    #[error("gc: {0}")]
    Span(packstore::Error),
    /// A packstore error surfaced unchanged, as in Go: the mark's `get`,
    /// `liveness`, `segments`, or the sweep's `compact`.
    #[error(transparent)]
    Objects(packstore::Error),
    /// A refstore error surfaced unchanged, as in Go (`refs.All`).
    #[error(transparent)]
    Refs(refstore::Error),
    /// A [`fstree::child_keys`] failure surfaced unchanged from a tree walk,
    /// as in Go.
    #[error(transparent)]
    Children(fstree::ChildKeysError),
}

impl Error {
    /// Reports the overlapping-cycle refusal (Go: `errors.Is(err,
    /// ErrCycleRunning)`).
    pub fn is_cycle_running(&self) -> bool {
        matches!(self, Error::CycleRunning)
    }

    /// Reports a cycle canceled by [`Collector::close`] or
    /// [`Collector::wipe`] (Go: `errors.Is(err, context.Canceled)`).
    pub fn is_canceled(&self) -> bool {
        matches!(self, Error::Canceled)
    }
}

/// Paces the copier to `rate` bytes/s; a non-positive rate never sleeps.
/// Compact calls [`Throttle::pace`] from its single append loop, so one
/// throttle bounds the aggregate (Go: `throttle`; its mutex has no
/// counterpart here — the pace callback is a `FnMut` owned exclusively by
/// that loop).
struct Throttle {
    rate: i64,
    start: Instant,
    bytes: i64,
}

impl Throttle {
    /// A throttle whose clock starts now (Go: `newThrottle`).
    fn new(rate: i64) -> Throttle {
        Throttle {
            rate,
            start: Instant::now(),
            bytes: 0,
        }
    }

    /// Accounts `n` copied bytes and sleeps off any time the copy is ahead
    /// of the configured rate (Go: `pace`).
    fn pace(&mut self, n: usize) {
        if self.rate <= 0 {
            return;
        }
        self.bytes += n as i64;
        let owed = throttle_owed(self.bytes, self.rate);
        let elapsed = self.start.elapsed();
        if owed > elapsed {
            std::thread::sleep(owed - elapsed);
        }
    }
}

/// The total time a copy of `n` bytes at `rate` bytes/s should have taken.
/// Divide-before-multiply: `n * 1e9` overflows a 64-bit count past ~8.6 GiB
/// (Go: `throttleOwed`). `n` and `rate` are positive when called.
fn throttle_owed(n: i64, rate: i64) -> Duration {
    Duration::from_secs((n / rate) as u64)
        + Duration::from_nanos(((n % rate) * 1_000_000_000 / rate) as u64)
}

/// Reports whether the filesystem holding `path` has less than `min` bytes
/// free; `min` 0 means 5 % of the filesystem. A failed probe (statfs error,
/// or a path no C string can name) reports `false` — it never triggers
/// pressure (Go: `freeBelow` over `unix.Statfs`).
///
/// The probe is `libc::statfs`, mirroring Go's `unix.Statfs` field for field
/// (`f_bavail`/`f_bsize`/`f_blocks`); POSIX `statvfs` was rejected because
/// Darwin's `fsblkcnt_t` there is 32-bit, truncating block counts on large
/// volumes.
#[allow(clippy::unnecessary_cast)] // the statfs field types differ per target
fn free_below(path: &Path, min: u64) -> bool {
    use std::os::unix::ffi::OsStrExt;
    let Ok(cpath) = std::ffi::CString::new(path.as_os_str().as_bytes()) else {
        return false;
    };
    // SAFETY: `cpath` is a valid NUL-terminated C string and `st` is a
    // properly aligned, zero-initialized statfs buffer (all-zero bytes are a
    // valid value for every field); statfs only writes into it, and it is
    // read only after the call reports success.
    let mut st: libc::statfs = unsafe { std::mem::zeroed() };
    if unsafe { libc::statfs(cpath.as_ptr(), &mut st) } != 0 {
        return false;
    }
    let bsize = st.f_bsize as u64;
    let mut min = min;
    if min == 0 {
        min = (st.f_blocks as u64).saturating_mul(bsize) / 20;
    }
    (st.f_bavail as u64).saturating_mul(bsize) < min
}

/// Locks a mutex, neutralizing poisoning like packstore's `unpoison`: a
/// panicked holder leaves the state no more suspect than Go's, which has no
/// poisoning.
fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Read-locks an `RwLock`, neutralizing poisoning (see [`lock`]).
fn read_lock<T>(l: &RwLock<T>) -> RwLockReadGuard<'_, T> {
    l.read().unwrap_or_else(PoisonError::into_inner)
}

/// Write-locks an `RwLock`, neutralizing poisoning (see [`lock`]).
fn write_lock<T>(l: &RwLock<T>) -> RwLockWriteGuard<'_, T> {
    l.write().unwrap_or_else(PoisonError::into_inner)
}
