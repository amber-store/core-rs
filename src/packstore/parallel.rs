//! Parallel batch writing: a bounded worker pool with dedup and optional
//! pre-commit BLAKE3 verification (Go: `packstore/parallel.go`).

use std::collections::HashSet;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Mutex, mpsc};
use std::thread;

use crate::key::{self, Key};

use super::prepare::prepare;
use super::{Error, Object, Store, unpoison};

/// The byte threshold at which a writer fsyncs the active segment, making
/// everything appended so far durable (Go: `DefaultBatchSize`).
pub const DEFAULT_BATCH_SIZE: usize = 16 << 20; // 16 MiB

/// Summarizes one [`Store::write_parallel`] run. On an error, the stats
/// reflect the work done before the abort (Go: `WriteStats`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct WriteStats {
    /// Objects newly written.
    pub stored: usize,
    /// Objects skipped (already present, or duplicated in the stream).
    pub deduped: usize,
    /// Payload bytes of newly-written objects (uncompressed).
    pub bytes_stored: u64,
}

/// Configures [`Store::write_parallel`] (Go: `WriteOpts`).
#[derive(Debug, Clone, Copy, Default)]
pub struct WriteOpts {
    /// Concurrent writers; 0 means the available parallelism (Go:
    /// `GOMAXPROCS`).
    pub writers: usize,
    /// Fsync when a writer has appended this many bytes; 0 means
    /// [`DEFAULT_BATCH_SIZE`].
    pub batch_size: usize,
    /// Recompute and check each new object's key before storing it.
    pub verify: bool,
}

/// A concurrency-safe set of keys, sharded on the key's last byte (uniformly
/// distributed) to spread lock contention across writers (Go: `seenSet`).
struct SeenSet {
    shards: [Mutex<HashSet<Key>>; 256],
}

impl SeenSet {
    fn new() -> SeenSet {
        SeenSet {
            shards: std::array::from_fn(|_| Mutex::new(HashSet::new())),
        }
    }

    /// Records `k` and reports true if it was not already present (Go:
    /// `addIfAbsent`).
    fn add_if_absent(&self, k: Key) -> bool {
        unpoison(self.shards[k.as_bytes()[key::SIZE - 1] as usize].lock()).insert(k)
    }
}

/// Shared bookkeeping for one `write_parallel` run.
struct Run {
    cancel: AtomicBool,
    first_err: Mutex<Option<Error>>,
    stored: AtomicU64,
    deduped: AtomicU64,
    bytes_stored: AtomicU64,
    seen: SeenSet,
}

impl Run {
    /// Records the run's first error and cancels the distributor and sibling
    /// workers (Go: errgroup's first error + `cancel()`).
    fn fail(&self, e: Error) {
        let mut slot = unpoison(self.first_err.lock());
        if slot.is_none() {
            *slot = Some(e);
        }
        drop(slot);
        self.cancel.store(true, Ordering::Relaxed);
    }

    fn canceled(&self) -> bool {
        self.cancel.load(Ordering::Relaxed)
    }
}

impl Store {
    /// Stores every object the iterator yields using multiple concurrent
    /// workers. Compression and (optional) verification run in parallel;
    /// appends serialize on the active segment. Each worker fsyncs after
    /// appending `batch_size` bytes, and the run fsyncs once more before
    /// returning.
    ///
    /// Like [`Store::write_batch`], it is durable-on-return but NOT atomic:
    /// on error or crash a valid prefix remains, which a content-addressed
    /// re-run deduplicates. The final fsync also covers dedup hits against
    /// records a concurrent, uncommitted run appended. If the iterator
    /// yields an error, the run stops and returns it. With `verify`, a
    /// key/payload mismatch stops the run with an [`Error::Verify`]. An
    /// [`Object`] may carry a pre-encoded `record` instead of `data`; it is
    /// validated and appended as is (see [`Object`]).
    ///
    /// Returns the stats alongside the outcome because, exactly like Go's
    /// `(WriteStats, error)` pair, an erroring run still reports the work
    /// done before the abort (Go: `WriteParallel`).
    pub fn write_parallel<I, E>(&self, seq: I, opts: WriteOpts) -> (WriteStats, Result<(), Error>)
    where
        I: IntoIterator<Item = Result<Object, E>>,
        I::IntoIter: Send,
        E: std::error::Error + Send + Sync + 'static,
    {
        let _write_token = self.begin_write();
        let writers = if opts.writers == 0 {
            thread::available_parallelism().map_or(1, |n| n.get())
        } else {
            opts.writers
        };
        let batch_size = if opts.batch_size == 0 {
            DEFAULT_BATCH_SIZE
        } else {
            opts.batch_size
        };

        let run = Run {
            cancel: AtomicBool::new(false),
            first_err: Mutex::new(None),
            stored: AtomicU64::new(0),
            deduped: AtomicU64::new(0),
            bytes_stored: AtomicU64::new(0),
            seen: SeenSet::new(),
        };
        let (tx, rx) = mpsc::sync_channel::<Object>(writers * 2);
        let rx = Mutex::new(rx);
        let seq = seq.into_iter();

        thread::scope(|s| {
            // Distributor: forward objects from the iterator to the worker
            // pool. A yielded error cancels the run and becomes its error.
            s.spawn(|| {
                let tx = tx; // move the sender in; dropping it closes the channel
                for item in seq {
                    match item {
                        Err(e) => {
                            run.fail(Error::Source(Box::new(e)));
                            return;
                        }
                        Ok(obj) => {
                            if run.canceled() || tx.send(obj).is_err() {
                                return;
                            }
                        }
                    }
                }
            });
            for _ in 0..writers {
                s.spawn(|| self.run_writer(&rx, &run, batch_size, opts.verify));
            }
        });

        let mut err = unpoison(run.first_err.into_inner());
        // Always fsync, as write_batch does. Even with nothing appended a
        // dedup hit may have matched another run's unsynced record. On error
        // the appended prefix is visible and must become durable too.
        if let Err(serr) = self.sync_active()
            && err.is_none()
        {
            err = Some(serr);
        }
        let stats = WriteStats {
            stored: run.stored.load(Ordering::Relaxed) as usize,
            deduped: run.deduped.load(Ordering::Relaxed) as usize,
            bytes_stored: run.bytes_stored.load(Ordering::Relaxed),
        };
        (stats, err.map_or(Ok(()), Err))
    }

    /// Consumes objects, encoding (compressing, optionally verifying) them
    /// concurrently with its siblings and appending them to the store. It
    /// fsyncs after every `batch_size` appended bytes. The final fsync is
    /// `write_parallel`'s (Go: `runWriter`).
    fn run_writer(
        &self,
        rx: &Mutex<mpsc::Receiver<Object>>,
        run: &Run,
        batch_size: usize,
        verify: bool,
    ) {
        let mut pending = 0usize;
        loop {
            if run.canceled() {
                return; // no flush; the run-level final sync covers us
            }
            let recv = unpoison(rx.lock()).recv();
            let obj = match recv {
                Ok(o) => o,
                Err(_) => break, // channel closed: input exhausted
            };
            // Observe before the per-writer dedup: a barrier capture must
            // grey dedup hits too. Cross-writer duplicates may be observed
            // more than once — harmless, the grey set is a set.
            self.observe(obj.key);
            if !run.seen.add_if_absent(obj.key) {
                run.deduped.fetch_add(1, Ordering::Relaxed);
                continue;
            }
            match self.has(obj.key) {
                Ok(true) => {
                    run.deduped.fetch_add(1, Ordering::Relaxed);
                    continue;
                }
                Ok(false) => {}
                Err(e) => return run.fail(e),
            }
            let key = obj.key;
            let (rec, ulen) = match prepare(obj, verify) {
                Ok(v) => v,
                Err(e) => return run.fail(e),
            };
            if let Err(e) = self.append(key, &rec, false) {
                return run.fail(e);
            }
            run.stored.fetch_add(1, Ordering::Relaxed);
            run.bytes_stored.fetch_add(ulen, Ordering::Relaxed);
            pending += rec.len();
            if pending >= batch_size {
                pending = 0;
                if let Err(e) = self.sync_active() {
                    return run.fail(e);
                }
            }
        }
    }
}
