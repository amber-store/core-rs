//! The pre-build sizing pass (Go: `ingest/scan.go`).

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::thread;

use crate::amberignore::{Matcher, descend_opt, ignored_opt};

use super::driver::{DirEnt, read_dir_sorted};
use super::parallel::Sem;
use super::{Error, Exclude, Opts, lock};

/// Walks the directory tree at `dir` concurrently (readdir + lstat only, no
/// content reads) and returns the number of regular files and the total size
/// of their content — the totals a progress display is sized by. Only
/// regular-file bytes are counted, since only regular files are read during a
/// build; symlinks, dirs and special files contribute nothing. The first
/// stat/read error aborts the scan.
///
/// When `no_ignore` is false, `.amberignore` filtering rooted at `dir` is
/// applied exactly as a build applies it, so the totals match what the build
/// reads.
///
/// The fan-out mirrors the parallel build: each entry may run on a pooled
/// thread, but when the pool is full the work runs inline on the current
/// thread, so the recursion cannot deadlock waiting on a slot held by a
/// descendant (Go: `Scan`).
pub fn scan(dir: impl AsRef<Path>, no_ignore: bool, jobs: usize) -> Result<(u64, u64), Error> {
    let dir = dir.as_ref();
    let ign = if !no_ignore {
        Some(Matcher::root(dir).map_err(|e| Error::ignore_load(dir, e))?)
    } else {
        None
    };
    scan_tree(dir, ign.as_ref(), jobs)
}

/// [`scan`] with every relevant option of `opts` applied — `no_ignore`,
/// `jobs` and `exclude` — so that the totals match what
/// [`objects`](super::objects) reads with the same options (Go: `ScanWith`).
pub fn scan_with(dir: impl AsRef<Path>, opts: &Opts) -> Result<(u64, u64), Error> {
    let dir = dir.as_ref();
    let ign = if !opts.no_ignore {
        Some(Matcher::root(dir).map_err(|e| Error::ignore_load(dir, e))?)
    } else {
        None
    };
    run(
        dir,
        ign.as_ref(),
        opts.jobs(),
        Exclude::new(dir, opts.exclude_set()),
    )
}

/// Implements [`scan`] over an explicit matcher (`None` ingests everything)
/// (Go: `scanTree`).
pub(crate) fn scan_tree(
    dir: &Path,
    ign: Option<&Matcher>,
    jobs: usize,
) -> Result<(u64, u64), Error> {
    run(dir, ign, jobs.max(1), Exclude::none(dir))
}

/// Runs one scan with a resolved worker count and exclusion list.
fn run(
    dir: &Path,
    ign: Option<&Matcher>,
    jobs: usize,
    exclude: Exclude<'_>,
) -> Result<(u64, u64), Error> {
    let s = Scanner {
        files: AtomicU64::new(0),
        bytes: AtomicU64::new(0),
        sem: Sem::new(jobs),
        first_err: Mutex::new(None),
        exclude,
    };
    s.walk(dir, ign);
    if let Some(e) = lock(&s.first_err).take() {
        return Err(e);
    }
    Ok((
        s.files.load(Ordering::Relaxed),
        s.bytes.load(Ordering::Relaxed),
    ))
}

/// Shared state of one scan (Go: `scanner`).
struct Scanner<'a> {
    files: AtomicU64,
    bytes: AtomicU64,
    sem: Sem,
    first_err: Mutex<Option<Error>>,
    /// Implements [`Opts::exclude`] for [`scan_with`] (Go: `root`/`exclude`).
    exclude: Exclude<'a>,
}

impl Scanner<'_> {
    fn set_err(&self, e: Error) {
        let mut slot = lock(&self.first_err);
        if slot.is_none() {
            *slot = Some(e);
        }
    }

    fn walk(&self, dir: &Path, ign: Option<&Matcher>) {
        let ents = match read_dir_sorted(dir) {
            Ok(v) => v,
            Err(e) => return self.set_err(e),
        };
        thread::scope(|s| {
            for de in ents {
                if self.exclude.skips(dir, &de.name) {
                    continue;
                }
                if ignored_opt(ign, &de.name, de.is_dir) {
                    continue;
                }
                let full = de.join(dir);
                let work = move || self.entry(full, de, ign);
                if self.sem.try_acquire() {
                    s.spawn(move || {
                        work();
                        self.sem.release();
                    });
                } else {
                    work();
                }
            }
        });
    }

    fn entry(&self, full: PathBuf, de: DirEnt, ign: Option<&Matcher>) {
        let md = match fs::symlink_metadata(&full) {
            Ok(v) => v,
            Err(source) => {
                return self.set_err(Error::Io {
                    op: "lstat",
                    path: full,
                    source,
                });
            }
        };
        let ft = md.file_type();
        if ft.is_dir() {
            let sub = match descend_opt(ign, &full, &de.name) {
                Ok(v) => v,
                Err(e) => return self.set_err(Error::ignore_load(&full, e)),
            };
            self.walk(&full, sub.as_ref());
        } else if ft.is_file() {
            self.files.fetch_add(1, Ordering::Relaxed);
            self.bytes.fetch_add(md.len(), Ordering::Relaxed);
        }
        // else: symlink, device, socket, fifo — not read during ingest.
    }
}
