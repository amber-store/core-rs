//! The concurrent tree builder (Go: `ingest/parallel.go`).

use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::thread;

use crate::amberignore::{Matcher, ignored_opt};
use crate::fstree::{self, DirBuilder};
use crate::key::Key;

use super::driver::{Driver, Emit, read_dir_sorted};
use super::{Error, Exclude};

/// A non-blocking counting semaphore (Go: the `chan struct{}` used with a
/// non-blocking send).
pub(crate) struct Sem {
    free: AtomicUsize,
}

impl Sem {
    pub(crate) fn new(n: usize) -> Sem {
        Sem {
            free: AtomicUsize::new(n),
        }
    }

    /// Takes a slot if one is free, without blocking.
    pub(crate) fn try_acquire(&self) -> bool {
        self.free
            .fetch_update(Ordering::Acquire, Ordering::Relaxed, |v| v.checked_sub(1))
            .is_ok()
    }

    pub(crate) fn release(&self) {
        self.free.fetch_add(1, Ordering::Release);
    }
}

/// Builds the CAS tree concurrently. It reuses the driver's per-entry and
/// per-file logic but fans the directory walk out across a bounded pool:
/// each directory entry's subtree (a file's chunks or a subdirectory) is
/// built independently, then the directory's own leaf/index objects are
/// assembled in the original sorted-entry order. The sink is the
/// (concurrency-safe) emit shared by all workers (Go: `pbuilder`).
pub(crate) struct PBuilder<'a> {
    d: &'a Driver,
    sink: &'a dyn Emit,
    /// Bounds the number of in-flight worker threads. Offloading uses a
    /// non-blocking acquire: when the pool is full, the work runs inline on
    /// the current thread, so a parent never blocks waiting for a slot held
    /// by one of its own descendants — the recursion cannot deadlock.
    sem: Sem,
    /// Implements [`super::Opts::exclude`]: names skipped at the root
    /// directory only (Go: `root`/`exclude`).
    exclude: Exclude<'a>,
}

impl<'a> PBuilder<'a> {
    pub(crate) fn new(
        d: &'a Driver,
        sink: &'a dyn Emit,
        jobs: usize,
        exclude: Exclude<'a>,
    ) -> PBuilder<'a> {
        PBuilder {
            d,
            sink,
            sem: Sem::new(jobs),
            exclude,
        }
    }

    /// Builds the directory at `path` and returns its root key. Entries
    /// excluded by `ign` — or, at the root, by the exclude list — are skipped
    /// (excluded directories are pruned without being read). Sibling entries
    /// are built concurrently; the directory's leaf/index objects are then
    /// emitted in sorted-entry order, identical to the sequential walk (Go:
    /// `pbuilder.buildDir`).
    pub(crate) fn build_dir(&self, path: &Path, ign: Option<&Matcher>) -> Result<Key, Error> {
        let ents = read_dir_sorted(path)?;
        let kept: Vec<_> = ents
            .into_iter()
            .filter(|de| !self.exclude.skips(path, &de.name))
            .filter(|de| !ignored_opt(ign, &de.name, de.is_dir))
            .collect();

        let mut results: Vec<Option<Result<fstree::Entry, Error>>> = Vec::new();
        results.resize_with(kept.len(), || None);
        // The scope joins every spawned sibling before returning, mirroring
        // Go's per-directory WaitGroup.
        thread::scope(|s| {
            for (slot, de) in results.iter_mut().zip(&kept) {
                let full = de.join(path);
                let mut build = move || {
                    *slot = Some(self.build_one(&full, &de.name, ign));
                };
                if self.sem.try_acquire() {
                    s.spawn(move || {
                        build();
                        self.sem.release();
                    });
                } else {
                    build();
                }
            }
        });

        let mut db = DirBuilder::new(self.d.ic);
        for r in results {
            // Every slot was filled above; the first error in entry order
            // wins, exactly as in Go.
            let e = match r {
                Some(res) => res?,
                None => return Err(Error::Stopped), // unreachable
            };
            db.add_entry(&mut |o| self.sink.emit(o), e)
                .map_err(Error::from_build)?;
        }
        db.finish(&mut |o| self.sink.emit(o))
            .map_err(Error::from_build)
    }

    /// Builds one directory entry, recursing through this builder for
    /// subdirectories.
    fn build_one(
        &self,
        full: &Path,
        name: &[u8],
        ign: Option<&Matcher>,
    ) -> Result<fstree::Entry, Error> {
        self.d
            .build_entry(full, name, ign, self.sink, &|p, i| self.build_dir(p, i))
    }
}
