//! The collector: lifecycle, the reference hooks, and the mark walk (Go:
//! `gc/collector.go`).

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, RwLock, RwLockReadGuard, mpsc};
use std::thread;
use std::time::Duration;

use crate::fstree::{self, child_keys};
use crate::key::{Key, Type};
use crate::packstore::{self, MarkSet};
use crate::reference::Reference;
use crate::refstore;

use super::{CycleStats, Error, Options, lock, read_lock};

/// The test hook slot (Go: `midMark`, a `func()`). An `Arc` rather than a
/// `Box` so the cycle can copy it out under `mu` and invoke it with no lock
/// held, exactly as Go copies the func value.
pub(super) type MidMark = Arc<dyn Fn() + Send + Sync>;

/// A cycle's cancellation signal: the per-cycle flag published under `mu`
/// (tripped by [`Collector::close`] and [`Collector::wipe`]) plus, for
/// background cycles, the loop's stop flag (Go: the per-cycle context
/// derived from the caller's or the loop's context).
#[derive(Clone, Copy)]
pub(super) struct Cancel<'a> {
    cycle: Option<&'a AtomicBool>,
    parent: Option<&'a AtomicBool>,
}

impl Cancel<'_> {
    /// No cancellation source — [`Collector::status`]'s advisory mark (Go:
    /// the caller-supplied context, which the ported API does not take).
    pub(super) const NONE: Cancel<'static> = Cancel {
        cycle: None,
        parent: None,
    };

    /// The signal for one cycle (Go: the context `Run` derives).
    pub(super) fn new<'a>(cycle: &'a AtomicBool, parent: Option<&'a AtomicBool>) -> Cancel<'a> {
        Cancel {
            cycle: Some(cycle),
            parent,
        }
    }

    /// Whether either flag has been tripped (Go: `ctx.Err() != nil`).
    pub(super) fn is_canceled(self) -> bool {
        self.cycle.is_some_and(|f| f.load(Ordering::Relaxed))
            || self.parent.is_some_and(|f| f.load(Ordering::Relaxed))
    }
}

/// State guarded by [`Core::mu`] (Go: the `Collector` fields under `mu`).
pub(super) struct MuState {
    /// The last cycle's stats, recorded on success and failure (Go: `last`).
    pub(super) last: Option<CycleStats>,
    /// The last cycle's error text, if it failed (Go: `lastErr`; only its
    /// `Error()` string is ever read back, so the port stores the string).
    pub(super) last_err: Option<String>,
    /// Cancels the running cycle (Go: `cancelCycle`).
    pub(super) cancel_cycle: Option<Arc<AtomicBool>>,
    /// Test hook: runs after the mark, before the sweep (Go: `midMark`).
    pub(super) mid_mark: Option<MidMark>,
}

/// The collector state shared with the background loop thread (Go: the
/// `Collector` struct; split out because the loop holds its own `Arc`).
pub(super) struct Core {
    pub(super) objects: Arc<packstore::Store>,
    pub(super) refs: Arc<refstore::Store>,
    /// Former closures dir: kept for layout compat and the free-space probe.
    pub(super) dir: PathBuf,
    pub(super) opts: Options,

    /// The reference lock (Go: `refLock`): a reference PUT holds it shared
    /// from its completeness walk to its commit; a cycle holds it
    /// exclusively around the roots snapshot and again around the sweep.
    /// Between the two a PUT proceeds — its walked closure joins the write
    /// barrier's grey set, so the sweep keeps it.
    pub(super) ref_lock: RwLock<()>,

    /// Guards [`MuState`] (Go: `mu`).
    pub(super) mu: Mutex<MuState>,

    /// Held for the whole cycle; cycles never overlap (Go: `cycleMu`).
    pub(super) cycle_mu: Mutex<()>,

    /// Stop flag the background loop's cycles treat as a parent cancel (Go:
    /// the loop context; [`Collector::close`] cancels it).
    pub(super) loop_cancel: AtomicBool,
}

/// The background loop thread and its stop channel (Go: the `loop`
/// goroutine with `stop`/`done`).
struct Background {
    /// Dropping the sender disconnects the loop's receiver, waking and
    /// stopping it (Go: `stop`).
    stop: mpsc::Sender<()>,
    /// Joined by [`Collector::close`] (Go: `<-done`).
    handle: thread::JoinHandle<()>,
}

/// Implements the cycle and the reference hooks over an open packstore and
/// refstore pair. A process has one per pair; other processes may run their
/// own on the same store (`architecture/mark-sweep-gc.md`, "Across
/// processes"). Close it before the stores (Go: `Collector`). The stores are held as `Arc`s so the
/// background loop can outlive the caller's borrows; see `port-notes/gc.md`.
pub struct Collector {
    pub(super) core: Arc<Core>,
    bg: Mutex<Option<Background>>,
}

impl Collector {
    /// Opens the collector next to an already-open packstore and refstore.
    /// `dir` is the layout slot the simple-gc collector kept closure files
    /// in (`<store>/closures`): it is created empty and any leftover closure
    /// state from a previous collector is swept — closures were derived
    /// data. With `opts.interval > 0` a thread runs a cycle per interval
    /// until [`Collector::close`] (Go: `Open`).
    pub fn open(
        dir: impl AsRef<Path>,
        objects: Arc<packstore::Store>,
        refs: Arc<refstore::Store>,
        opts: Options,
    ) -> Result<Collector, Error> {
        let dir = dir.as_ref().to_path_buf();
        let opts = opts.with_defaults();
        std::fs::create_dir_all(&dir).map_err(|source| Error::Creating {
            dir: dir.clone(),
            source,
        })?;
        for entry in std::fs::read_dir(&dir).map_err(Error::Io)? {
            let entry = entry.map_err(Error::Io)?;
            // Go's os.RemoveAll takes files and directories alike; std
            // splits that into two calls.
            let is_dir = entry.file_type().map_err(Error::Sweep)?.is_dir();
            let removed = if is_dir {
                std::fs::remove_dir_all(entry.path())
            } else {
                std::fs::remove_file(entry.path())
            };
            removed.map_err(Error::Sweep)?;
        }
        let core = Arc::new(Core {
            objects,
            refs,
            dir,
            opts,
            ref_lock: RwLock::new(()),
            mu: Mutex::new(MuState {
                last: None,
                last_err: None,
                cancel_cycle: None,
                mid_mark: None,
            }),
            cycle_mu: Mutex::new(()),
            loop_cancel: AtomicBool::new(false),
        });
        let bg = if opts.interval > Duration::ZERO {
            Some(spawn_loop(Arc::clone(&core)))
        } else {
            None
        };
        Ok(Collector {
            core,
            bg: Mutex::new(bg),
        })
    }

    /// Stops the background loop, cancels and waits out a running cycle.
    /// Close the collector before the stores under it. Idempotent; always
    /// returns `Ok` (Go: `Close`, which always returns nil).
    pub fn close(&self) -> Result<(), Error> {
        if let Some(bg) = lock(&self.bg).take() {
            // Go's stop() cancels the loop context, which the running
            // background cycle's context derives from; here the loop's
            // parent flag plus the per-cycle flag reproduce that, and must
            // be tripped before the join or it would wait out a full
            // uncancelled cycle.
            self.core.loop_cancel.store(true, Ordering::Relaxed);
            drop(bg.stop);
            self.core.cancel_running();
            let _ = bg.handle.join();
        }
        self.core.cancel_running();
        // Barrier: wait out a running cycle (Go: cycleMu.Lock(); Unlock()).
        drop(lock(&self.core.cycle_mu));
        Ok(())
    }

    /// Cancels a running cycle, waits it out and then runs `reset` (the
    /// store wipe — [`packstore::Store::wipe`] and [`refstore::Store::wipe`])
    /// while holding the cycle slot. The mark reads segment mmaps unpinned,
    /// so the stores must not be wiped under it (Go: `Wipe`).
    pub fn wipe<E>(&self, reset: impl FnOnce() -> Result<(), E>) -> Result<(), E> {
        self.core.cancel_running();
        let _cycle = lock(&self.core.cycle_mu); // wait out the cancelled cycle
        reset()?;
        let mut st = lock(&self.core.mu);
        st.last = None;
        st.last_err = None;
        Ok(())
    }

    /// Readies a reference PUT naming `root` under the reference lock, held
    /// shared until commit or abort: the tree is walked for completeness (a
    /// missing object aborts with the error naming it — the caller's 404)
    /// and the walked closure is handed to the write barrier, so a PUT
    /// landing while a mark runs cannot lose its objects to the sweep.
    /// Exactly one of [`PreparedRef::commit`] (after the reference record
    /// is stored) or [`PreparedRef::abort`] (it was not) should be called;
    /// dropping the handle is an abort. Release of an old root needs no
    /// bookkeeping; [`Collector::release_ref`] exists for symmetry (Go:
    /// `PrepareRef`).
    pub fn prepare_ref(&self, root: Key) -> Result<PreparedRef<'_>, Error> {
        let span = self.begin_span()?;
        // An early return drops the span — Go's sp.End() on the error path.
        self.core.prepare(root)?;
        Ok(PreparedRef { _span: Some(span) })
    }

    /// Gates one object-write span (an ingest, a pull, an inbox drain)
    /// against the sweep. The span holds the reference lock shared: the sweep
    /// waits out in-flight writes, and a write stalls while a sweep runs —
    /// never during the mark, which writers pass behind the write barrier.
    /// Without the gate a dedup hit against a record in a condemned pack
    /// could report success and then lose the record to the pack's removal.
    /// Drop the returned guard when the span's writes are durable (Go:
    /// `BeginWrite`).
    pub fn begin_write(&self) -> WriteGate<'_> {
        match self.begin_span() {
            Ok(span) => WriteGate {
                _span: Some(span),
                _ref_lock: None,
            },
            // The store is closing, which the span's writes will report. The
            // reference lock's half of the span is kept all the same.
            Err(_) => WriteGate {
                _span: None,
                _ref_lock: Some(read_lock(&self.core.ref_lock)),
            },
        }
    }

    /// Opens a write span. End it — drop it — when its writes are durable and
    /// its references are stored (Go: `BeginSpan`).
    pub fn begin_span(&self) -> Result<Span<'_>, Error> {
        self.core.begin_span()
    }

    /// Records that one reference naming `root` was deleted or overwritten.
    /// The mark-and-sweep collector keeps no per-root state, so this is a
    /// no-op: the next cycle simply no longer marks from the root (Go:
    /// `ReleaseRef`; call sites keep it for protocol parity).
    pub fn release_ref(&self, root: Key) -> Result<(), Error> {
        let _ = root;
        Ok(())
    }

    /// A second façade over the same collector, for test hooks that must
    /// call back into it (the Go test hook simply closes over `c`).
    #[cfg(test)]
    pub(super) fn test_handle(&self) -> Collector {
        Collector {
            core: Arc::clone(&self.core),
            bg: Mutex::new(None),
        }
    }
}

impl Drop for Collector {
    /// Closes on drop — [`packstore::Store`]'s precedent — so an undropped
    /// background loop cannot outlive the collector. Go has no finalizer;
    /// explicit [`Collector::close`] remains the contract.
    fn drop(&mut self) {
        let _ = self.close();
    }
}

/// A reference PUT prepared under the collector's reference lock, held
/// shared until consumed (Go: the `commit`/`abort` closures returned by
/// `PrepareRef` — a single `sync.Once`-guarded release; consuming `self`
/// gives the same exactly-once guarantee).
#[derive(Debug)]
#[must_use = "call commit after storing the reference record, or abort"]
pub struct PreparedRef<'c> {
    /// The span [`Collector::prepare_ref`] opened for this one PUT; `None`
    /// when the PUT was prepared inside a span the caller holds.
    _span: Option<Span<'c>>,
}

impl PreparedRef<'_> {
    /// Releases the reference lock after the reference record was stored
    /// (Go: `commit`).
    pub fn commit(self) {}

    /// Releases the reference lock without a stored record (Go: `abort`).
    /// Dropping the handle without calling either method is equivalent.
    pub fn abort(self) {}
}

/// An open write span: object writes and the reference puts that name them,
/// as one unit against the sweep, in this process and in every other. A sweep
/// here or elsewhere waits for it to end, and it waits for one. Inside it
/// references are prepared with [`Span::prepare_ref`], which takes no lock
/// again: [`Collector::prepare_ref`] inside a span would take the reference
/// lock a second time, and wait for ever behind a cycle that is waiting for
/// the span.
///
/// [`Collector::begin_span`] takes the reference lock and then the store's
/// gate, the order a cycle takes them in. A span opened on the store itself
/// ([`packstore::Store::begin_write`]) and held across
/// [`Collector::prepare_ref`] takes them the other way round; in a process
/// that runs cycles the two can wait for each other.
///
/// Dropping the span ends it; a span that has ended cannot be used, which Go
/// checks at run time (Go: `Span`).
#[must_use = "the span ends when this is dropped"]
pub struct Span<'c> {
    core: &'c Core,
    // In this order: the store's span ends first, then the reference lock
    // goes (Go: `sp.end()`, then `refLock.RUnlock()`).
    _span: packstore::WriteSpan<'c>,
    _guard: RwLockReadGuard<'c, ()>,
}

impl std::fmt::Debug for Span<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Span").finish_non_exhaustive()
    }
}

impl Span<'_> {
    /// Readies a reference PUT naming `root`, as [`Collector::prepare_ref`]
    /// does, under the locks the span already holds. The reference may be
    /// stored any time before the span ends; the returned handle's `commit`
    /// and `abort` are there for the shape and release nothing (Go:
    /// `Span.PrepareRef`).
    pub fn prepare_ref(&self, root: Key) -> Result<PreparedRef<'_>, Error> {
        self.core.prepare(root)?;
        Ok(PreparedRef { _span: None })
    }

    /// [`Collector::release_ref`] (Go: `Span.ReleaseRef`).
    pub fn release_ref(&self, root: Key) -> Result<(), Error> {
        let _ = root;
        Ok(())
    }

    /// Closes the span (Go: `End`). Dropping it does the same.
    pub fn end(self) {}
}

/// What [`Collector::begin_write`] holds until it is dropped.
#[derive(Debug)]
#[must_use = "the write span ends when this is dropped"]
pub struct WriteGate<'c> {
    /// The whole span, or, when the store would not open one, the reference
    /// lock's half of it.
    _span: Option<Span<'c>>,
    _ref_lock: Option<RwLockReadGuard<'c, ()>>,
}

impl Core {
    /// Trips the running cycle's cancel flag, if one is running (Go:
    /// `c.cancelCycle()` under `mu`).
    pub(super) fn cancel_running(&self) {
        let st = lock(&self.mu);
        if let Some(flag) = &st.cancel_cycle {
            flag.store(true, Ordering::Relaxed);
        }
    }

    /// See [`Collector::begin_span`].
    pub(super) fn begin_span(&self) -> Result<Span<'_>, Error> {
        let guard = read_lock(&self.ref_lock);
        // The same span across processes (packstore's gate): a cycle in
        // another store waits for it, and it waits for one. The barrier that
        // lets a PUT land during a mark lives in the cycle's own process, so
        // a PUT from elsewhere must not land between a cycle's roots snapshot
        // and its sweep at all. An early return drops `guard`.
        let span = self.objects.begin_write().map_err(Error::Span)?;
        Ok(Span {
            core: self,
            _span: span,
            _guard: guard,
        })
    }

    /// The work of a reference PUT under the reference lock: the completeness
    /// walk, and the walked closure handed to the write barrier (Go:
    /// `prepare`).
    pub(super) fn prepare(&self, root: Key) -> Result<(), Error> {
        let objects = &self.objects;
        let keys =
            fstree::check_complete(root, |k| objects.get(k), |k| objects.has(k), self.opts.jobs)
                .map_err(|source| Error::Walk {
                    root,
                    source: Box::new(source),
                })?;
        self.objects.observe_keys(&keys);
        Ok(())
    }

    /// Lists the root key of every reference. The caller snapshots under the
    /// reference lock when the result must be exact (Go: `roots`).
    pub(super) fn roots(&self) -> Result<Vec<Key>, Error> {
        let recs = self.refs.all().map_err(Error::Refs)?;
        let mut roots = Vec::with_capacity(recs.len());
        for r in recs {
            let decoded = Reference::decode(&r.data).map_err(|e| Error::Reference {
                name: r.name.clone(),
                source: Box::new(e),
            })?;
            let root = Key::parse(&decoded.key).map_err(|e| Error::Reference {
                name: r.name.clone(),
                source: Box::new(e),
            })?;
            roots.push(root);
        }
        Ok(roots)
    }

    /// Walks every root into a fresh mark set. The roots must be a snapshot
    /// taken under the reference lock; the walk touches only
    /// snapshot-reachable objects and runs concurrently with ingests (their
    /// writes join the barrier's grey set) (Go: `markLive`).
    pub(super) fn mark_live(&self, cancel: Cancel<'_>, roots: &[Key]) -> Result<MarkSet, Error> {
        let mut live = self.objects.new_mark_set();
        for &root in roots {
            self.mark_from(cancel, &mut live, root)?;
        }
        Ok(live)
    }

    /// Prunes at already-marked keys, so shared subtrees are walked once.
    /// Blob and xattr payloads are marked without being read (Go:
    /// `markFrom`).
    fn mark_from(&self, cancel: Cancel<'_>, live: &mut MarkSet, root: Key) -> Result<(), Error> {
        let mut stack = vec![root];
        for n in 0usize.. {
            if stack.is_empty() {
                break;
            }
            // Every 1024 pops, including the first (Go: `n%1024 == 0`).
            if n % 1024 == 0 && cancel.is_canceled() {
                return Err(Error::Canceled);
            }
            let k = stack.pop().expect("stack is non-empty");
            let (newly, present) = live.mark(k);
            if !present {
                return Err(Error::MissingFromStore { key: k });
            }
            if !newly {
                continue;
            }
            if matches!(k.type_(), Type::Blob | Type::XattrSet) {
                continue;
            }
            let data = self.objects.get(k).map_err(Error::Objects)?;
            let children = child_keys(k, &data).map_err(Error::Children)?;
            stack.extend(children);
        }
        Ok(())
    }
}

/// The background cycle loop: one policy cycle per `interval` until the
/// stop channel disconnects; errors land in [`Status::last_error`] only
/// (Go: `loop` over a `time.Ticker`; `recv_timeout` stands in for the
/// ticker+select, so intervals here separate cycle *ends* rather than
/// starts — see `port-notes/gc.md`).
///
/// [`Status::last_error`]: super::Status::last_error
fn spawn_loop(core: Arc<Core>) -> Background {
    let (stop, ticks) = mpsc::channel::<()>();
    let handle = thread::spawn(move || {
        loop {
            match ticks.recv_timeout(core.opts.interval) {
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    if core.loop_cancel.load(Ordering::Relaxed) {
                        return; // Go: the select's ctx.Done arm
                    }
                    let _ = core.run(-1.0, Some(&core.loop_cancel), u64::MAX);
                }
                Ok(()) | Err(mpsc::RecvTimeoutError::Disconnected) => return,
            }
        }
    });
    Background { stop, handle }
}
