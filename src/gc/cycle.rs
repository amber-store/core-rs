//! One cycle: snapshot the reference roots behind the write barrier; mark
//! everything they reach into a bitmap over the packs' footer indexes,
//! concurrently with ingests; sweep by rewriting every eligible pack whose
//! dead ratio crosses the line ([`crate::packstore::Store::compact`]) under
//! the reference lock. Cycles never overlap. See `specs/gc.qnt` in the Go
//! repo for the barrier protocol (Go: `gc/cycle.go`).

use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crate::packstore::CompactOpts;

use super::collector::{Cancel, Core};
use super::{Collector, Error, MIN_FREE_GARBAGE, Throttle, free_below, lock, write_lock};

/// Describes one cycle (Go: `CycleStats`).
#[derive(Debug, Clone, PartialEq)]
pub struct CycleStats {
    /// Wall-clock cycle start (Go: `Start`).
    pub start: SystemTime,
    /// Total cycle time (Go: `Duration`).
    pub duration: Duration,
    /// Roots snapshot + mark walk (Go: `MarkDuration`).
    pub mark_duration: Duration,
    /// Compact: select, copy, delete (Go: `SweepDuration`).
    pub sweep_duration: Duration,
    /// The dead-ratio line this cycle used (Go: `Threshold`).
    pub threshold: f64,
    /// Distinct live objects marked (Go: `Marked`).
    pub marked: usize,
    /// Sealed packs considered (Go: `Scored`).
    pub scored: usize,
    /// Victim ids, ascending (Go: `Reaped`).
    pub reaped: Vec<u64>,
    /// Records rewritten out of the victims (Go: `CopiedRecords`).
    pub copied_records: usize,
    /// Bytes rewritten out of the victims (Go: `CopiedBytes`, an `int64`;
    /// the count cannot be negative).
    pub copied_bytes: u64,
    /// Victim file bytes freed on disk, footers included (Go: `FreedBytes`,
    /// an `int64`).
    pub freed_bytes: u64,
}

impl Collector {
    /// Marks from every reference root and sweeps the eligible packs above
    /// the line: `garbage >= 0` forces that line, `garbage < 0` uses policy
    /// — 0.5, or 0.1 under min-free pressure. Ingests and reads run through
    /// the mark (the write barrier keeps them); reference publication and
    /// the sweep stall each other on the reference lock. An overlapping call
    /// is refused with [`Error::CycleRunning`], never queued (Go: `Run`;
    /// the context maps to an internal cancel flag tripped by
    /// [`Collector::close`] / [`Collector::wipe`]).
    pub fn run(&self, garbage: f64) -> Result<CycleStats, Error> {
        self.core.run(garbage, None)
    }
}

impl Core {
    /// See [`Collector::run`]. `parent` is the background loop's stop flag,
    /// when the loop is the caller (Go: the ctx parameter).
    pub(super) fn run(
        &self,
        garbage: f64,
        parent: Option<&AtomicBool>,
    ) -> Result<CycleStats, Error> {
        let _cycle = match self.cycle_mu.try_lock() {
            Ok(g) => g,
            Err(std::sync::TryLockError::WouldBlock) => return Err(Error::CycleRunning),
            Err(std::sync::TryLockError::Poisoned(p)) => p.into_inner(),
        };
        let cancel = Arc::new(AtomicBool::new(false));
        lock(&self.mu).cancel_cycle = Some(Arc::clone(&cancel));
        let (stats, res) = self.cycle(Cancel::new(&cancel, parent), garbage);
        // Recorded on success and failure, then the cancel slot is cleared —
        // all before the cycle lock is released (Go: the mu block + defers).
        let mut st = lock(&self.mu);
        st.last = Some(stats.clone());
        st.last_err = res.as_ref().err().map(|e| e.to_string());
        st.cancel_cycle = None;
        drop(st);
        res.map(|()| stats)
    }

    /// One cycle body with the total-duration accounting (Go: `cycle` and
    /// its deferred `stats.Duration` store).
    fn cycle(&self, cancel: Cancel<'_>, garbage: f64) -> (CycleStats, Result<(), Error>) {
        let t0 = Instant::now();
        let mut stats = CycleStats {
            start: SystemTime::now(),
            duration: Duration::ZERO,
            mark_duration: Duration::ZERO,
            sweep_duration: Duration::ZERO,
            threshold: 0.0,
            marked: 0,
            scored: 0,
            reaped: Vec::new(),
            copied_records: 0,
            copied_bytes: 0,
            freed_bytes: 0,
        };
        let res = self.cycle_body(cancel, garbage, t0, &mut stats);
        stats.duration = t0.elapsed();
        (stats, res)
    }

    fn cycle_body(
        &self,
        cancel: Cancel<'_>,
        garbage: f64,
        t0: Instant,
        stats: &mut CycleStats,
    ) -> Result<(), Error> {
        let mut threshold = garbage;
        if threshold < 0.0 {
            threshold = self.opts.garbage;
            // Note: the free-space probe runs on the closures dir path, as
            // in Go.
            if free_below(&self.dir, self.opts.min_free) {
                threshold = MIN_FREE_GARBAGE;
            }
        }
        stats.threshold = threshold;

        // Snapshot: barrier on, then the roots, under the reference lock —
        // a PUT in flight commits or aborts before the snapshot; every
        // later PUT greys its walked closure, every later ingest its
        // written keys.
        //
        // Other processes are held to the simpler safe policy, quiesce: from
        // here to the end of the cycle the store's gate keeps their writers
        // and their reference PUTs out (`packstore::Store::begin_sweep`), and
        // the view taken now is complete and stable. The gate is taken and
        // dropped under the reference lock, with no local span in flight,
        // because a file lock cannot be turned from shared into exclusive
        // atomically.
        let (sweep, roots) = {
            let _ref = write_lock(&self.ref_lock);
            let sweep =
                self.objects
                    .begin_sweep(&|| cancel.is_canceled())
                    .map_err(|e| match e {
                        crate::packstore::Error::GateCanceled => Error::Canceled,
                        e => Error::Objects(e),
                    })?;
            self.objects.begin_barrier();
            match self.roots() {
                Ok(roots) => (sweep, roots),
                Err(e) => {
                    self.objects.abort_barrier();
                    drop(sweep); // under the reference lock
                    return Err(e);
                }
            }
        };

        let live = self.mark_live(cancel, &roots);
        // The test hook runs after the mark returns but before its error
        // check, with no lock held (Go: the midMark read under mu).
        let hook = lock(&self.mu).mid_mark.clone();
        if let Some(hook) = hook {
            hook();
        }
        if let Ok(live) = &live {
            stats.marked = live.marked();
            stats.mark_duration = t0.elapsed(); // includes the snapshot
        }

        // Sweep, excluding reference publication. Compact consumes the grey
        // set, seals the active segment, rewrites the victims and deletes
        // them once the copies are durable. A failed or cancelled mark comes
        // through here as well: the gate goes under the reference lock.
        let _ref = write_lock(&self.ref_lock);
        let _sweep = sweep; // declared after `_ref`, so dropped before it
        let live = match live {
            Ok(live) if !cancel.is_canceled() => live,
            Ok(_) => {
                self.objects.abort_barrier();
                return Err(Error::Canceled);
            }
            Err(e) => {
                self.objects.abort_barrier();
                return Err(e);
            }
        };
        let mut opts = CompactOpts {
            min_dead_ratio: threshold,
            // Go's `time.Now().Add(-grace)` cannot fail; `SystemTime` can
            // underflow on absurd grace values, where the epoch — sparing
            // every real pack — is the conservative stand-in.
            horizon: Some(
                SystemTime::now()
                    .checked_sub(self.opts.grace)
                    .unwrap_or(UNIX_EPOCH),
            ),
            pace: None,
        };
        if self.opts.rate > 0 {
            let mut throttle = Throttle::new(self.opts.rate); // clock starts here
            opts.pace = Some(Box::new(move |n| throttle.pace(n)));
        }
        let sweep_start = Instant::now();
        let res = self.objects.compact(|k| live.contains(k), opts);
        stats.sweep_duration = sweep_start.elapsed();
        match res {
            Ok(cs) => {
                stats.scored = cs.segments_scanned;
                stats.reaped = cs.victims;
                stats.copied_records = cs.records_copied;
                stats.copied_bytes = cs.bytes_copied;
                stats.freed_bytes = cs.bytes_freed;
                Ok(())
            }
            // Go maps the partial CompactStats even on error; the Rust
            // compact returns no stats with its error (see
            // port-notes/packstore-gc.md), so the sweep counters stay zero.
            Err(e) => Err(Error::Objects(e)),
        }
    }
}
