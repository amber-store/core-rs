//! Builds content-addressed filesystem trees from a local directory or a
//! single regular file: it walks the source, applies `.amberignore`
//! filtering, splits file content by content-defined chunking, and streams
//! every built object (children before parents) to the consumer. [`objects`]
//! exposes the raw object stream for consumers that route objects themselves;
//! [`dir`] writes the stream straight into a packstore.
//!
//! This is a port of the Go `ingest` package. The stream shape differs only
//! in mechanics: Go returns an `iter.Seq2[fstree.Object, error]` plus a root
//! pointer filled in after the stream is drained; here [`objects`] returns an
//! [`ObjectStream`] iterator plus a [`Root`] handle with the same contract.

mod driver;
mod meta;
mod parallel;
mod scan;
mod xattrs;

#[cfg(test)]
mod tests;

use std::collections::HashSet;
use std::ffi::OsString;
use std::fs;
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};
use std::thread;

use crate::amberignore::Matcher;
use crate::chunkers::{self, ItemChunker};
use crate::fstree::{self, BuildError};
use crate::key::Key;
use crate::packstore;

use driver::{ChanSink, Driver, Stopped};
use parallel::PBuilder;
pub use scan::{scan, scan_with};

/// The item-chunker bit width used when [`ChunkOpts`] leaves it zero:
/// directory leaves and file index nodes average 2^7 entries (Go:
/// `DefaultItemBits`).
pub const DEFAULT_ITEM_BITS: u32 = 7;

/// The inline-xattr cap used when [`ChunkOpts`] leaves it zero: encoded
/// xattrs larger than this many bytes spill to an XattrSet object (Go:
/// `DefaultXattrInlineMax`).
pub const DEFAULT_XATTR_INLINE_MAX: usize = 256;

/// Receives build-progress events. Implementations must be safe for
/// concurrent use; no progress sink in [`Opts`] disables reporting. Totals
/// for sizing a progress display come from [`scan`].
pub trait Progress: Send + Sync {
    /// Records one regular file fully chunked.
    fn file_done(&self);
    /// Records `n` source-content bytes chunked.
    fn add_bytes(&self, n: usize);
}

/// Sets the content-defined-chunking parameters. The default value selects
/// the library defaults; the parameters determine every object key, so two
/// builds of the same tree agree only when their `ChunkOpts` agree.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ChunkOpts {
    /// Byte-chunker sizes; `None` selects the library defaults (Go: `Byte`,
    /// a nil-able pointer).
    pub byte: Option<chunkers::ByteOpts>,
    /// Item chunker average run = 2^bits; 0 selects [`DEFAULT_ITEM_BITS`].
    pub item_bits: u32,
    /// Xattr spill threshold in bytes; 0 selects
    /// [`DEFAULT_XATTR_INLINE_MAX`].
    pub xattr_inline_max: usize,
}

/// Configures a tree build (Go: `Opts`).
#[derive(Clone, Default)]
pub struct Opts {
    /// Bounds the concurrent build workers; 0 selects the available
    /// parallelism (Go: `<1` selects GOMAXPROCS).
    pub jobs: usize,
    /// The chunking parameters.
    pub chunk: ChunkOpts,
    /// Disables `.amberignore` filtering (which only applies to directory
    /// builds).
    pub no_ignore: bool,
    /// When set, receives build-progress events.
    pub progress: Option<Arc<dyn Progress>>,
    /// Names directly under the root that are never ingested, whatever
    /// `no_ignore` says. It applies to the root directory only; the same
    /// name deeper in the tree is ingested normally (Go: `Exclude []string`;
    /// names are compared bytewise against directory entries).
    pub exclude: Vec<OsString>,
}

impl std::fmt::Debug for Opts {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Opts")
            .field("jobs", &self.jobs)
            .field("chunk", &self.chunk)
            .field("no_ignore", &self.no_ignore)
            .field("progress", &self.progress.is_some())
            .field("exclude", &self.exclude)
            .finish()
    }
}

impl Opts {
    /// Resolves the effective worker count (Go: `Opts.jobs`).
    fn jobs(&self) -> usize {
        if self.jobs < 1 {
            thread::available_parallelism().map_or(1, |n| n.get())
        } else {
            self.jobs
        }
    }

    /// Turns `exclude` into a set; `None` when empty (Go: `Opts.excludeSet`).
    fn exclude_set(&self) -> Option<HashSet<Vec<u8>>> {
        if self.exclude.is_empty() {
            return None;
        }
        Some(self.exclude.iter().map(|n| n.as_bytes().to_vec()).collect())
    }

    /// Builds a driver from the chunking options (Go: `Opts.driver`).
    fn driver(&self) -> Driver {
        let bits = if self.chunk.item_bits == 0 {
            DEFAULT_ITEM_BITS
        } else {
            self.chunk.item_bits
        };
        let inline_max = if self.chunk.xattr_inline_max == 0 {
            DEFAULT_XATTR_INLINE_MAX
        } else {
            self.chunk.xattr_inline_max
        };
        Driver {
            ic: ItemChunker::new(bits),
            byte_opts: self.chunk.byte.clone(),
            xattr_inline_max: inline_max,
            progress: self.progress.clone(),
        }
    }
}

/// Errors from a tree build. File-system failures carry the failing path the
/// way Go's `*fs.PathError` does; chunker-option and fstree-encoder errors
/// are surfaced unchanged, exactly as the Go package returns them.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// A filesystem operation failed (Go: `*fs.PathError`, same `op path:
    /// cause` shape).
    #[error("{op} {}: {source}", path.display())]
    Io {
        /// The failing operation (`stat`, `lstat`, `open`, `readlink`, …).
        op: &'static str,
        /// The path the operation failed on.
        path: PathBuf,
        /// The underlying I/O error.
        source: io::Error,
    },
    /// Invalid [`chunkers::ByteOpts`], surfaced unchanged (Go:
    /// `chunkers.SplitBytes` returns the validation error as-is).
    #[error(transparent)]
    ChunkOpts(chunkers::OptionsError),
    /// The build path is neither a regular file nor a directory (Go:
    /// `statPath`).
    #[error("{} is neither a regular file nor a directory", path.display())]
    NotFileOrDir {
        /// The rejected path.
        path: PathBuf,
    },
    /// A walked entry has an unsupported file type (Go: `buildEntry`; the
    /// mode is the `S_IFMT` type bits, rendered like Go's `%#o`).
    #[error("{}: unsupported file type 0{mode:o}", path.display())]
    Unsupported {
        /// The entry's path.
        path: PathBuf,
        /// The entry's `st_mode & S_IFMT` type bits.
        mode: u64,
    },
    /// An fstree encoder failed (surfaced unchanged, as in Go).
    #[error(transparent)]
    Fstree(fstree::Error),
    /// `fstree: IndexBuilder.Finish with no children` — unreachable through
    /// this module (every file index receives at least one Blob), kept
    /// instead of panicking.
    #[error("fstree: IndexBuilder.Finish with no children")]
    NoChildren,
    /// Internal marker: the consumer stopped pulling from the object stream
    /// early (Go: `errStopped`). Never returned by the public API.
    #[error("ingest: consumer stopped")]
    Stopped,
    /// The packstore rejected a write (from [`dir`] only).
    #[error(transparent)]
    Store(packstore::Error),
}

impl Error {
    /// Maps a builder failure: encoder errors surface unchanged, an emit
    /// failure means the consumer stopped.
    fn from_build(e: BuildError<Stopped>) -> Error {
        match e {
            BuildError::Encode(e) => Error::Fstree(e),
            BuildError::Emit(Stopped) => Error::Stopped,
            BuildError::NoChildren => Error::NoChildren,
        }
    }

    /// Wraps an `.amberignore` load failure the way Go's `os.ReadFile`
    /// surfaces it: an open error on the ignore file itself.
    fn ignore_load(dir: &Path, source: io::Error) -> Error {
        Error::Io {
            op: "open",
            path: dir.join(crate::amberignore::FILE_NAME),
            source,
        }
    }
}

/// The root-only exclusion of [`Opts::exclude`], resolved against the build
/// root: a name in the set is skipped when the directory being read is `root`
/// (Go: the `root`/`exclude` pair carried by `pbuilder` and `scanner`).
pub(crate) struct Exclude<'a> {
    root: &'a Path,
    names: Option<HashSet<Vec<u8>>>,
}

impl<'a> Exclude<'a> {
    pub(crate) fn new(root: &'a Path, names: Option<HashSet<Vec<u8>>>) -> Exclude<'a> {
        Exclude { root, names }
    }

    /// No exclusions (Go: a zero `root`/nil `exclude`).
    pub(crate) fn none(root: &'a Path) -> Exclude<'a> {
        Exclude::new(root, None)
    }

    /// Reports whether `name`, an entry of directory `dir`, is excluded (Go:
    /// `path == b.root && b.exclude[name]`).
    pub(crate) fn skips(&self, dir: &Path, name: &[u8]) -> bool {
        self.names
            .as_ref()
            .is_some_and(|names| dir == self.root && names.contains(name))
    }
}

/// Locks `m`, ignoring poisoning: a panicking worker already aborts the
/// build, and the guarded state stays consistent.
fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// The root key of a completed build: set once the matching [`ObjectStream`]
/// has been consumed to completion without error (the Rust shape of Go's
/// `*key.Key` result).
#[derive(Clone, Debug)]
pub struct Root(Arc<OnceLock<Key>>);

impl Root {
    /// Returns the resolved root key, or `None` while the build has not
    /// completed successfully.
    pub fn get(&self) -> Option<Key> {
        self.0.get().copied()
    }
}

/// The stream of every CAS object a build produces, children before parents
/// with the root last. Yields a build error as its final item instead; drop
/// it early to abort the build. Built objects stream from the build workers
/// through a bounded channel, so production and consumption overlap.
#[derive(Debug)]
pub struct ObjectStream {
    rx: mpsc::Receiver<fstree::Object>,
    err: Arc<Mutex<Option<Error>>>,
    done: bool,
}

impl Iterator for ObjectStream {
    type Item = Result<fstree::Object, Error>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.done {
            return None;
        }
        match self.rx.recv() {
            Ok(o) => Some(Ok(o)),
            // Channel closed: the producer finished (and stored the root) or
            // failed (and stored the error). Both happen before the sender is
            // dropped, so the order here is safe.
            Err(mpsc::RecvError) => {
                self.done = true;
                lock(&self.err).take().map(Err)
            }
        }
    }
}

/// Builds the tree at `path` — a directory, or a single regular file — and
/// returns the stream of every CAS object it produces plus a handle to the
/// root key, which is set once the stream has been consumed to completion
/// without error. A directory root is a DirNode; a file root is the file's
/// content key (Blob or FileNode).
///
/// Object order is unspecified — the store is a flat content-addressed bag
/// and dedups by key — but per-file chunk order and per-directory entry order
/// are preserved, so every object's key, and the root, are deterministic
/// functions of the source tree and the chunking options (Go: `Objects`).
pub fn objects(path: impl AsRef<Path>, opts: Opts) -> Result<(ObjectStream, Root), Error> {
    let path = path.as_ref().to_path_buf();
    let is_dir = stat_path(&path)?;
    let ign = if is_dir && !opts.no_ignore {
        Some(Matcher::root(&path).map_err(|e| Error::ignore_load(&path, e))?)
    } else {
        None
    };
    let d = opts.driver();
    let jobs = opts.jobs();
    let exclude = opts.exclude_set();

    // Built objects stream to the consumer through a buffered channel, so
    // production and consumption overlap (Go: `objects` with bufSize jobs*2).
    let (tx, rx) = mpsc::sync_channel((jobs * 2).max(1));
    let root = Arc::new(OnceLock::new());
    let err = Arc::new(Mutex::new(None));

    let root_w = Arc::clone(&root);
    let err_w = Arc::clone(&err);
    let spawn_path = path.clone();
    thread::Builder::new()
        .name("ingest-build".into())
        .spawn(move || {
            let sink = ChanSink::new(tx);
            let res = if is_dir {
                let b = PBuilder::new(&d, &sink, jobs, Exclude::new(&path, exclude));
                b.build_dir(&path, ign.as_ref())
            } else {
                let r = d.build_file(&path, &sink);
                if r.is_ok() {
                    d.file_done();
                }
                r
            };
            match res {
                Ok(k) => {
                    let _ = root_w.set(k);
                }
                // Stopped means the consumer quit; not a real error (Go:
                // errStopped).
                Err(Error::Stopped) => {}
                Err(e) => *lock(&err_w) = Some(e),
            }
            // `sink` (the only sender) drops here, closing the channel after
            // the root/error is recorded.
        })
        .map_err(|source| Error::Io {
            op: "spawn",
            path: spawn_path,
            source,
        })?;

    Ok((
        ObjectStream {
            rx,
            err,
            done: false,
        },
        Root(root),
    ))
}

/// Builds the tree at `path` — a directory, or a single regular file — and
/// stores every object in `st` via [`packstore::Store::write_parallel`],
/// returning the write stats alongside the resolved root key. Like Go's
/// `(key, stats, err)` triple, an erroring run still reports the work done
/// before the abort (Go: `Dir`).
pub fn dir(
    st: &packstore::Store,
    path: impl AsRef<Path>,
    opts: Opts,
) -> (packstore::WriteStats, Result<Key, Error>) {
    let writers = opts.jobs; // raw, exactly as Go passes Opts.Jobs through
    let (stream, root) = match objects(path, opts) {
        Ok(v) => v,
        Err(e) => return (packstore::WriteStats::default(), Err(e)),
    };
    let (stats, res) = st.write_parallel(
        stream.map(|r| r.map(packstore::Object::from)),
        packstore::WriteOpts {
            writers,
            ..Default::default()
        },
    );
    match res {
        Ok(()) => match root.get() {
            Some(k) => (stats, Ok(k)),
            // Unreachable: a fully-drained, error-free stream implies the
            // producer stored the root. Kept instead of panicking.
            None => (stats, Err(Error::Stopped)),
        },
        // A build error travels through write_parallel as a source error;
        // unwrap it so callers see the ingest error itself, as in Go.
        Err(packstore::Error::Source(b)) => match b.downcast::<Error>() {
            Ok(e) => (stats, Err(*e)),
            Err(b) => (stats, Err(Error::Store(packstore::Error::Source(b)))),
        },
        Err(e) => (stats, Err(Error::Store(e))),
    }
}

/// Reports whether `path` is a directory. It errors if `path` does not exist
/// or is neither a regular file nor a directory (symlinks are followed) (Go:
/// `statPath`).
fn stat_path(path: &Path) -> Result<bool, Error> {
    let md = fs::metadata(path).map_err(|source| Error::Io {
        op: "stat",
        path: path.to_path_buf(),
        source,
    })?;
    if md.is_dir() {
        Ok(true)
    } else if md.file_type().is_file() {
        Ok(false)
    } else {
        Err(Error::NotFileOrDir {
            path: path.to_path_buf(),
        })
    }
}
