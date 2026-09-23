//! `gc.lock`: writers against a sweep, across processes (Go:
//! `packstore/gate.go`).
//!
//! A GC cycle must not lose an object that a writer just relied on: written,
//! or skipped as a duplicate of a record the cycle is about to reap. Within
//! one process the write barrier and the collector's reference lock see to
//! that (`specs/gc.qnt` in the Go repo, policy "barrier"). Across processes
//! the gate applies the simpler safe policy, "quiesce", with one flock:
//!
//!   - shared, while any write span of this store is in flight: every
//!     exported write, and the spans callers bracket with
//!     [`Store::begin_write`] (a completeness walk followed by a reference
//!     put);
//!   - exclusive, for a whole GC cycle, and for anything else that deletes
//!     segments ([`Store::compact`], [`Store::wipe`], [`Store::remove`]).
//!
//! So while one store sweeps, writers in other processes wait; readers never
//! do. Writers of the sweeping store itself do not touch the file lock while
//! their store holds it exclusively: the barrier and the reference lock deal
//! with them, as before.
//!
//! flock cannot convert between shared and exclusive atomically — the lock is
//! dropped in between, and another process may take it — so the gate never
//! converts: exclusive is taken and dropped only with no local span in
//! flight.
//!
//! Whoever takes the lock exclusively moves the generation on first: it reads
//! the counter under the lock, writes one more, and only then deletes
//! anything, so a sweep that dies half way has still told everyone. A store
//! counts from the file, not from the value it remembers: another store may
//! have swept since it looked. A store that takes the shared lock and finds
//! the generation moved looks at the directory again before it does anything
//! else: it may still have a reaped segment mapped, and its duplicate check
//! would find objects there that are gone. So does a store on its first span,
//! whatever the counter says: it may have opened in the middle of a sweep and
//! listed a directory that was about to change.
//!
//! Spans nest. A span that finds another in flight joins it without waiting,
//! even when a sweep of this store is waiting for the spans to end: the inner
//! span may be the outer one's own work — a put inside a `begin_write`
//! bracket — and the two would wait for each other. A sweep therefore starts
//! at a moment with no span in flight; a caller that writes without pause
//! from many threads, and sweeps in the same process, quiesces its writers
//! itself, as the collector does with its reference lock.

use std::fs::{File, OpenOptions};
use std::io;
use std::os::fd::AsRawFd;
use std::os::unix::fs::FileExt;
use std::path::Path;
use std::sync::{Condvar, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use super::{Error, Store, unpoison};

/// The cross-process form of the collector's reference lock; it carries the
/// view generation in its first 8 bytes, big-endian (Go: `gateFile`).
pub(crate) const GATE_FILE: &str = "gc.lock";

/// Waits for the file lock poll, backing off to this (Go: `maxGatePoll`).
const MAX_GATE_POLL: Duration = Duration::from_millis(50);

/// A store that just swept leaves the lock alone for this long, two polls'
/// worth, before it sweeps again: it would otherwise take the lock back
/// within microseconds of dropping it, and a writer polling from another
/// process could wait as long as the sweeps keep coming (Go: `sweepYield`).
const SWEEP_YIELD: Duration = Duration::from_millis(100);

/// Reports that the caller gave up the wait (Go: `ctx.Err()`).
pub(crate) type Cancel<'a> = Option<&'a (dyn Fn() -> bool + Sync)>;

#[derive(Debug, Default)]
struct State {
    /// Local write spans in flight.
    shared: usize,
    /// The file lock is held shared on their behalf.
    flocked: bool,
    /// Depth: this store holds the file lock exclusively.
    exclusive: usize,
    /// One thread is changing the file lock's state; the others wait.
    busy: bool,
    /// New local spans wait: a sweep is coming, going, or at work.
    blocked: bool,
    /// The generation this store's view is good for.
    generation: u64,
    /// The view was brought up to date under the file lock at least once.
    looked: bool,
    /// When this store last dropped the exclusive lock.
    swept_at: Option<Instant>,
    closed: bool,
    /// Test hook: spans never refresh the view, which reproduces the losses
    /// that prevents.
    never_refresh: bool,
}

/// Go: `gate`.
#[derive(Debug)]
pub(crate) struct Gate {
    f: File,
    st: Mutex<State>,
    cond: Condvar,
}

/// Undoes a busy step that unwinds. Between setting `busy` and publishing
/// the outcome a span or a sweep runs the store's refresh; if that panics —
/// in Go it would take the process down — every later span and sweep of this
/// store would wait for `busy` for ever, and the file lock the step took
/// would stay held against every other store.
struct BusyStep<'a> {
    gate: &'a Gate,
    done: bool,
}

impl Drop for BusyStep<'_> {
    fn drop(&mut self) {
        if self.done {
            return;
        }
        // Nobody else in this process holds the file lock during a busy
        // step, so letting go of it takes it from nobody.
        self.gate.unlock();
        let mut st = self.gate.lock();
        st.busy = false;
        st.blocked = false;
        self.gate.cond.notify_all();
    }
}

impl Gate {
    /// Go: `openGate`.
    pub(crate) fn open(dir: &Path) -> Result<Gate, Error> {
        let f = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(dir.join(GATE_FILE))
            .map_err(|e| Error::Other(format!("packstore: {GATE_FILE}: {e}")))?;
        Ok(Gate {
            f,
            st: Mutex::new(State::default()),
            cond: Condvar::new(),
        })
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        unpoison(self.st.lock())
    }

    /// Go: `generation`.
    pub(crate) fn generation(&self) -> Result<u64, Error> {
        let mut b = [0u8; 8];
        let mut n = 0;
        while n < b.len() {
            match self.f.read_at(&mut b[n..], n as u64) {
                Ok(0) => break,
                Ok(m) => n += m,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(e) => return Err(Error::Other(format!("packstore: {GATE_FILE}: {e}"))),
            }
        }
        if n < b.len() {
            return Ok(0); // never swept
        }
        Ok(u64::from_be_bytes(b))
    }

    /// Moves the generation on, counting from what the file holds, and
    /// returns the new value. The caller holds the file lock exclusively. A
    /// store that cannot write it must not sweep: nobody would learn of it
    /// (Go: `bump`).
    fn bump(&self) -> Result<u64, Error> {
        let next = self.generation()?.wrapping_add(1);
        // Not synced: it only has to outlive the processes that are running.
        self.f
            .write_all_at(&next.to_be_bytes(), 0)
            .map_err(|e| Error::Other(format!("packstore: {GATE_FILE}: {e}")))?;
        Ok(next)
    }

    /// Takes the file lock in the given mode, trying without blocking so that
    /// the wait ends with `cancel` or with the store (Go: `poll`).
    fn poll(&self, how: libc::c_int, cancel: Cancel<'_>) -> Result<(), Error> {
        let mut delay = Duration::from_millis(1);
        loop {
            // SAFETY: plain flock(2) on a valid open fd; no memory is involved.
            let rc = unsafe { libc::flock(self.f.as_raw_fd(), how | libc::LOCK_NB) };
            if rc == 0 {
                return Ok(());
            }
            let e = io::Error::last_os_error();
            match e.raw_os_error() {
                Some(libc::EWOULDBLOCK | libc::EINTR) => {}
                _ => return Err(Error::Other(format!("packstore: {GATE_FILE}: {e}"))),
            }
            let st = self.lock();
            if st.closed {
                return Err(Error::Closed);
            }
            let (st, _) = unpoison(self.cond.wait_timeout(st, delay));
            if st.closed {
                return Err(Error::Closed);
            }
            drop(st);
            if cancel.is_some_and(|c| c()) {
                return Err(Error::GateCanceled);
            }
            delay = (delay * 2).min(MAX_GATE_POLL);
        }
    }

    fn unlock(&self) {
        // SAFETY: plain flock(2) on a valid open fd; no memory is involved.
        unsafe { libc::flock(self.f.as_raw_fd(), libc::LOCK_UN) };
    }

    /// Waits on the condition until `ok` reports true, `cancel` fires or the
    /// gate closes (Go: `waitLocked`).
    fn wait<'a>(
        &'a self,
        mut st: MutexGuard<'a, State>,
        cancel: Cancel<'_>,
        ok: impl Fn(&State) -> bool,
    ) -> Result<MutexGuard<'a, State>, Error> {
        while !ok(&st) {
            if st.closed {
                return Err(Error::Closed);
            }
            if cancel.is_some_and(|c| c()) {
                return Err(Error::GateCanceled);
            }
            st = match cancel {
                // Nothing wakes a waiter when its caller cancels, so look.
                Some(_) => unpoison(self.cond.wait_timeout(st, MAX_GATE_POLL)).0,
                None => unpoison(self.cond.wait(st)),
            };
        }
        if st.closed {
            return Err(Error::Closed);
        }
        Ok(st)
    }

    /// Opens a write span. It waits for a sweep of this store that is coming,
    /// going or at work — unless a span is in flight already, which it then
    /// joins (see the module's comment). `refresh` brings the store's view up
    /// to date (Go: `beginShared`).
    pub(crate) fn begin_shared(
        &self,
        refresh: impl FnOnce() -> Result<(), Error>,
    ) -> Result<(), Error> {
        let mut st = self.wait(self.lock(), None, |s| {
            !s.busy && (!s.blocked || s.shared > 0)
        })?;
        if st.exclusive > 0 || st.flocked {
            st.shared += 1;
            return Ok(());
        }
        st.busy = true; // the first span takes the file lock; the rest wait for it here
        let (known, looked, never) = (st.generation, st.looked, st.never_refresh);
        drop(st);
        let mut step = BusyStep {
            gate: self,
            done: false,
        };

        let res = self.poll(libc::LOCK_SH, None).and_then(|()| {
            let r = self.generation().and_then(|g| {
                if (g != known || !looked) && !never {
                    refresh()?; // before any span proceeds to a duplicate check
                }
                Ok(g)
            });
            if r.is_err() {
                self.unlock();
            }
            r
        });
        let mut st = self.lock();
        st.busy = false;
        step.done = true;
        if let Ok(g) = &res {
            st.flocked = true;
            st.shared = 1;
            st.generation = *g;
            st.looked = true;
        }
        self.cond.notify_all();
        res.map(|_| ())
    }

    /// Go: `endShared`.
    pub(crate) fn end_shared(&self) {
        let mut st = self.lock();
        st.shared -= 1;
        if st.shared == 0 && st.flocked {
            self.unlock();
            st.flocked = false;
        }
        self.cond.notify_all();
    }

    /// Takes the gate for a sweep: it waits out this store's spans, then
    /// every other store's, and refreshes the view, which from then on is
    /// complete and stable. Calls nest: `compact` inside a collector's cycle
    /// finds the gate already held. It must not be called from inside a write
    /// span of the same store, which it would wait for (Go: `beginExclusive`).
    pub(crate) fn begin_exclusive(
        &self,
        cancel: Cancel<'_>,
        refresh: impl FnOnce() -> Result<(), Error>,
    ) -> Result<(), Error> {
        let mut st = self.wait(self.lock(), cancel, |s| !s.busy && !s.blocked)?;
        if st.exclusive > 0 {
            st.exclusive += 1;
            return Ok(());
        }
        st.blocked = true;
        let mut st = match self.wait(st, cancel, |s| s.shared == 0) {
            Ok(st) => st,
            Err(e) => {
                self.lock().blocked = false;
                self.cond.notify_all();
                return Err(e);
            }
        };
        st.busy = true;
        drop(st);
        let mut step = BusyStep {
            gate: self,
            done: false,
        };

        let res = self.poll(libc::LOCK_EX, cancel).and_then(|()| {
            // The generation first, before anything can be deleted; then the
            // view, before this store's own spans are let in again: they run
            // their duplicate checks against it.
            let r = self.bump().and_then(|g| refresh().map(|()| g));
            if r.is_err() {
                self.unlock();
            }
            r
        });
        let mut st = self.lock();
        st.busy = false;
        st.blocked = false;
        step.done = true;
        if let Ok(g) = &res {
            st.exclusive = 1;
            st.generation = *g;
            st.looked = true;
        }
        self.cond.notify_all();
        res.map(|_| ())
    }

    /// Go: `endExclusive`.
    pub(crate) fn end_exclusive(&self) {
        let mut st = self.lock();
        if st.exclusive > 1 {
            st.exclusive -= 1;
            return;
        }
        // Local writes ran under the exclusive lock without a file lock of
        // their own. None may be in flight when it goes, or another store
        // could sweep under them; and the lock cannot be turned into a shared
        // one.
        while (st.busy || st.blocked) && !st.closed {
            st = unpoison(self.cond.wait(st));
        }
        st.blocked = true;
        while st.shared > 0 && !st.closed {
            st = unpoison(self.cond.wait(st));
        }
        self.unlock();
        st.exclusive = 0;
        st.blocked = false;
        st.swept_at = Some(Instant::now());
        self.cond.notify_all();
    }

    /// Waits out what is left of [`SWEEP_YIELD`] since this store's last
    /// sweep (Go: `yield`).
    pub(crate) fn yield_to_writers(&self, cancel: Cancel<'_>) -> Result<(), Error> {
        let mut st = self.lock();
        if st.exclusive > 0 {
            return Ok(()); // nested: the lock is held, nobody is being kept waiting for it
        }
        let Some(until) = st.swept_at.map(|t| t + SWEEP_YIELD) else {
            return Ok(());
        };
        loop {
            if st.closed {
                return Err(Error::Closed);
            }
            if cancel.is_some_and(|c| c()) {
                return Err(Error::GateCanceled);
            }
            let left = until.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return Ok(());
            }
            st = unpoison(self.cond.wait_timeout(st, left.min(MAX_GATE_POLL))).0;
        }
    }

    /// Keeps this store's own write spans out, waiting for the ones in
    /// flight, until [`Gate::resume_local`]. `compact` runs inside the pair:
    /// a write's duplicate check must not race the removal of a segment, and
    /// writes that bypass the collector are held by nothing else (Go:
    /// `pauseLocal`).
    pub(crate) fn pause_local(&self) {
        let mut st = self.lock();
        while (st.busy || st.blocked) && !st.closed {
            st = unpoison(self.cond.wait(st));
        }
        st.blocked = true;
        while st.shared > 0 && !st.closed {
            st = unpoison(self.cond.wait(st));
        }
    }

    /// Go: `resumeLocal`.
    pub(crate) fn resume_local(&self) {
        self.lock().blocked = false;
        self.cond.notify_all();
    }

    /// Wakes every waiter with [`Error::Closed`] and drops whatever lock is
    /// held; the file itself closes with the store (Go: `close`).
    pub(crate) fn close(&self) {
        self.lock().closed = true;
        self.cond.notify_all();
        self.unlock();
    }

    /// Test hook: see [`State::never_refresh`].
    #[cfg(test)]
    pub(crate) fn set_never_refresh(&self, v: bool) {
        self.lock().never_refresh = v;
    }
}

/// An open write span; dropping it ends the span (Go: the `done` function
/// [`Store::begin_write`]'s counterpart returns).
#[derive(Debug)]
#[must_use = "the write span ends when this is dropped"]
pub struct WriteSpan<'a> {
    gate: &'a Gate,
}

impl Drop for WriteSpan<'_> {
    fn drop(&mut self) {
        self.gate.end_shared();
    }
}

/// The store taken for a sweep; dropping it lets the other stores' writers in
/// again (Go: the `done` function `BeginSweep` returns).
#[derive(Debug)]
#[must_use = "the sweep ends when this is dropped"]
pub struct Sweep<'a> {
    gate: &'a Gate,
}

impl Drop for Sweep<'_> {
    fn drop(&mut self) {
        self.gate.end_exclusive();
    }
}

/// [`Gate::pause_local`] for as long as it lives.
pub(crate) struct LocalPause<'a> {
    gate: &'a Gate,
}

impl Drop for LocalPause<'_> {
    fn drop(&mut self) {
        self.gate.resume_local();
    }
}

impl Store {
    /// Opens a write span: until it is dropped, no other store can sweep.
    /// Every exported write opens one itself; callers bracket larger spans
    /// that must not straddle a sweep, such as a completeness walk followed
    /// by a reference put. If another store has swept since this one last
    /// looked, the view is refreshed before `begin_write` returns. Spans
    /// nest: a write, or another `begin_write`, inside an open span joins it
    /// at no cost and never waits.
    ///
    /// A process that also runs a [`crate::gc::Collector`] on this store
    /// opens its spans through the collector (`Collector::begin_span`), which
    /// takes its reference lock before this gate, the order a cycle takes
    /// them in. A span opened here and held across `Collector::prepare_ref`
    /// takes them the other way round, and the two can wait for each other
    /// (Go: `BeginWrite`).
    pub fn begin_write(&self) -> Result<WriteSpan<'_>, Error> {
        self.gate.begin_shared(|| self.refresh())?;
        Ok(WriteSpan { gate: &self.gate })
    }

    /// Takes the store for a GC cycle: it waits for this store's write spans
    /// and every other store's to end, keeps other stores' writers out until
    /// the returned guard is dropped, and refreshes the view, which is then
    /// complete and stable. This store's own writers go on: the write barrier
    /// and the collector's reference lock are what holds them where needed.
    /// The view generation moves as soon as the lock is taken, so that every
    /// other store looks at the directory again before its next write, even
    /// if this process dies half way. `compact`, `wipe` and `remove` take the
    /// gate themselves; inside a `begin_sweep` they find it held. `cancel`
    /// ends the wait with an error when it reports true (Go: `BeginSweep`,
    /// whose context it stands for).
    pub fn begin_sweep(&self, cancel: &(dyn Fn() -> bool + Sync)) -> Result<Sweep<'_>, Error> {
        self.gate.yield_to_writers(Some(cancel))?;
        self.gate.begin_exclusive(Some(cancel), || self.refresh())?;
        Ok(Sweep { gate: &self.gate })
    }

    /// The exclusive gate for an operation of this store that deletes
    /// segments.
    pub(crate) fn sweep_gate(&self) -> Result<Sweep<'_>, Error> {
        self.gate.begin_exclusive(None, || self.refresh())?;
        Ok(Sweep { gate: &self.gate })
    }

    /// See [`Gate::pause_local`].
    pub(crate) fn pause_local_writers(&self) -> LocalPause<'_> {
        self.gate.pause_local();
        LocalPause { gate: &self.gate }
    }
}
