# ingest port notes

Go sources: `ingest/ingest.go`, `driver.go`, `parallel.go`, `scan.go`,
`meta.go`, `xattr_common.go`, `xattr_darwin.go`, `xattr_linux.go` (pinned
`e4fcb60`). Rust: `src/ingest/{mod,driver,parallel,scan,meta,xattrs}.rs`,
tests in `src/ingest/tests.rs` (full Go-test port) and `tests/ingest.rs`
(public-surface integration). Dev CLI: `examples/amber-store.rs`.

## API mapping

| Go | Rust |
|----|------|
| `Objects(path, opts) (iter.Seq2[Object,error], *key.Key, error)` | `objects(path, opts) -> Result<(ObjectStream, Root), Error>` — the stream is an `Iterator<Item = Result<fstree::Object, Error>>`; `Root::get()` returns `Some` only after a complete, error-free drain, mirroring the late-filled `*key.Key`. Dropping the stream early cancels the build (Go: `yield` returning false → context cancel). |
| `Dir(st, path, opts) (key.Key, WriteStats, error)` | `dir(st, path, opts) -> (WriteStats, Result<Key, Error>)` — stats are reported even for erroring runs, like Go's triple. |
| `Scan(dir, noIgnore, jobs) (int64, int64, error)` | `scan(dir, no_ignore, jobs) -> Result<(u64, u64), Error>` (sizes can't be negative; Go uses int64 only because `FileInfo.Size` does). |
| `Progress` interface | `trait Progress: Send + Sync`, carried as `Option<Arc<dyn Progress>>` in `Opts`. |
| `Opts.Jobs int` (<1 ⇒ GOMAXPROCS) | `jobs: usize` (0 ⇒ `available_parallelism`). Negative values are unrepresentable. |
| `ChunkOpts.ItemBits int` | `item_bits: u32` (a negative bit width was never usable). |
| `errStopped` | `Error::Stopped` — public in the enum but never returned by the public API; the producer filters it exactly as `objects` filters `errStopped`. |

One `Error` enum covers the whole package (Go returns untyped `error`s).
Filesystem failures carry `op path: cause` like `*fs.PathError`;
chunker-option and fstree errors pass through transparently. `dir()`
additionally unwraps a build error that traveled through
`write_parallel` as `packstore::Error::Source` (via downcast), so callers see
the same error Go's `Dir` returns; genuine store failures surface as
`Error::Store`.

## Ordering contract (parallel.go, read carefully)

What Go guarantees — and what this port reproduces exactly:

- **Stream order is unspecified overall**: sibling subtrees emit into the
  shared channel concurrently, so interleaving is scheduling-dependent
  (`Objects` doc: "Object order is unspecified").
- **Per-file chunk order is preserved**: one worker chunks a file
  sequentially, emitting Blob then FileNode levels bottom-up.
- **Per-directory determinism**: `pbuilder.buildDir` waits for all kept
  entries (WaitGroup ↔ `thread::scope`), then assembles DirLeaf/DirNode
  objects **in sorted-entry order** on one thread — identical objects and
  keys to the sequential walk. The first error **in entry order** wins (not
  chronologically), checked before each `AddEntry`, as in Go.
- **Children before parents; the root is the last object emitted.**
- Worker offload uses a non-blocking semaphore acquire with inline fallback
  (Go: non-blocking channel send), so a parent never waits on a slot held by
  a descendant — ported as an `AtomicUsize` try-acquire.
- Buffered channel of `jobs*2` between build and consumer (same constant).

Every key (and the root) is therefore a deterministic function of the source
tree and the chunk options; `dir_writes_to_packstore` asserts root equality
across jobs=1 and jobs=8, and the parity tests compare the full object set
against the sequential oracle.

## Metadata capture

- `lstat` via `std::fs::symlink_metadata` + `MetadataExt`: raw `st_mode`
  (u64), uid, gid, `mtime*1e9 + mtime_nsec` with wrapping arithmetic
  (`Time.UnixNano` overflows the same way).
- Type dispatch on `st_mode & S_IFMT` with libc constants (values match
  x/sys/unix on Linux and Darwin). Unsupported types render Go's `%#o`
  (`0140000`-style).
- Device numbers: hand-ported x/sys/unix `Major`/`Minor` for Linux
  (gnu_dev_major/minor) and Darwin (`>>24 & 0xff` / `& 0xffffff`), i.e. the
  exact functions Go calls — not the local C macros.
- Symlink targets via `read_link`, raw bytes (no UTF-8 assumption anywhere:
  names and targets are `Vec<u8>` end to end).

## Xattrs

- Darwin uses the follow variants (`Listxattr`/`Getxattr`), Linux the
  no-follow variants (`Llistxattr`/`Lgetxattr`); mirrored with the `xattr`
  crate's plain vs `_deref` functions. Only ever called on non-symlinks, so
  the distinction is unobservable — kept for fidelity.
- Error tolerance matches Go: any list/get failure aborts the build; empty
  names in the list are dropped (`splitXattrNames`); an attribute that
  vanishes between list and get is turned back into ENODATA/ENOATTR (the
  crate maps it to `None`).
- One unobservable-in-practice difference: Go's two-call size-probe errors
  with ERANGE if a value *grows* between the calls; the crate retries the
  fetch instead. Rust is strictly more tolerant in that race; keys are
  unaffected.
- Spill rule ported exactly: inline iff `len(encode_xattrs(m)) <=
  xattr_inline_max`, else an emitted `XattrSet` object referenced by key 9.
  Sorting by *encoded* key bytes lives in `cbor::encode_xattrs` (already
  golden-tested).

## Scan

Same fan-out pattern as the build (non-blocking semaphore, inline fallback,
per-directory join). First error is recorded and the walk continues (Go has
no cancellation there either); `.amberignore` filtering is applied exactly as
the build applies it, including `d_type`-based `is_dir` for the ignore check
and lstat-based dispatch afterwards.

## Sequential walk

`Driver::build_dir` (Go `driver.buildDir`) has no production caller — Go's
`Objects` always uses `pbuilder` for directories — but it is the reference
oracle for the parity tests, so it stays compiled (`#[allow(dead_code)]`
with a comment) rather than being test-gated.

## Golden vectors

There are no ingest-specific files under `tests/golden/`, and the VECTORS.md
golden tree cannot be materialized unprivileged (fixed uid/gid 1000, device
nodes, sockets), so there is no golden root-key test built from a
materialized directory. The gates are instead: the full ported Go test suite
(sequential/parallel parity, pruned-tree amberignore oracle, packstore
round-trip, progress totals) plus differential interop against the Go CLI
(below). The existing `tests/golden/fstree` tree continues to cover the
builder/codec byte-compatibility that ingest rides on.

## Dev CLI (`examples/amber-store.rs`)

Mirrors `cmd/amber-store` minus the progress UI (a hidden no-op
`--no-progress` keeps command lines interchangeable). Same store layout
(`<dir>/packstore`, `<dir>/refs`), `--store` / `$AMBER_STORE`, spec grammar
`KEY[/PATH]` | `ref:NAME[@PATH]`, chunk-flag semantics (defaults
32Ki/128Ki/256Ki are *set* values; all-zero selects library defaults;
partial zeros error), `ls -l` rendering (mode string, device `maj,min`
sizes, key-length sizes, `Jan _2 15:04` vs `Jan _2  2006` with the six-month
cutoff computed via `localtime_r`/`mktime` standing in for
`now.AddDate(0,-6,0)`), `ref` subcommands with RFC3339-UTC list output, and
byte-identical PAX export. Differences:

- `restore` spools the tar through an unlinked temp file instead of an
  in-process pipe: the workspace toolchain floor (rustc 1.86) predates
  `std::io::pipe`. Memory stays flat; an export error surfaces before
  extraction begins, like the Go pipe's `CloseWithError`.
  One visible consequence (found by the review of the Go PR #15 backport,
  which made more specs fail late): a failed `restore` leaves nothing at
  the destination, because the export fails before extraction starts. Go
  extracts while it exports and leaves the destination with whatever was
  written before the failure. Exit code, stdout and stderr are the same.
- Argument arity/usage errors come from clap instead of hand-rolled checks;
  store/spec/resolution errors print as `amber-store: <err>` with exit 1,
  matching Go's shape.
- Entry names/symlink targets are written to stdout as raw bytes, exactly as
  Go's `%s` on byte-backed strings.

## Interop verification (Go CLI ↔ Rust CLI, macOS)

Fresh stores per side, same source trees; root keys printed by
`go run ./cmd/amber-store ingest` and the Rust example were **identical** in
every case:

- mixed tree (multi-chunk 700 KiB file, empty file, symlink, fifo, nested
  `.amberignore` composition incl. negation/dir-only/pruning, inline xattr,
  mode 600): `220af4…214e`, and `ls` / `ls --keys` / `export` output
  byte-identical (`diff`/`cmp`); restore round-trip verified (contents,
  symlink, fifo, mode 600, xattr, pruned entries absent).
- spilled xattr (400 B > 256), xattr on a directory, setuid + sticky bits:
  parity, also at `-j 1`.
- custom options `--min 1024 --avg 4096 --max 16384 --item-bits 3
  --xattr-inline-max 64`: parity.
- single-file ingest (300 KiB): parity.
- unix socket entry, dangling symlink, pre-1970 mtime: parity, `ls`
  rendering (incl. `Jun  1  1969`) identical.

All scratch trees/stores lived under `/tmp/amber-interop` and were deleted;
the Go checkout was only read (`go run` from its own directory).

## Go PR #3 backport (2026-08-27)

ENOTSUP/EOPNOTSUPP from the xattr *list* call now means "no xattrs" (a
filesystem without xattr support), as in tar and rsync; get errors still
abort. Go refactored to an injectable `readXattrsWith(list, get)` for
testability; the port mirrors that as `read_xattrs_with` over the `xattr`
crate's closure shapes (an iterator-returning `list` instead of Go's
two-call size/fill protocol — the crate owns that dance).

## Go PR #11 backport (2026-09-16)

`Opts.Exclude []string` → `Opts::exclude: Vec<OsString>`: names directly
under the root that are never ingested, whatever `no_ignore` says. Root
only — the same name deeper in the tree is ingested normally (the consumer
is dstore, whose working copies keep `.dstore/` at the root of the tree they
push). Go carries a `root`/`exclude` pair on both `pbuilder` and `scanner`
and tests `path == root && exclude[name]` ahead of the `.amberignore`
check; the port folds that pair into one `Exclude` helper (`mod.rs`) shared
by `PBuilder` and `Scanner`, with the same check order and the same
"nil when empty" short-circuit (`Opts::exclude_set` → `None`). Names
compare bytewise (`OsStrExt::as_bytes`) against the sorted dirents, like
Go's string compare. `Path` equality is component-wise where Go compares
strings; the two cannot disagree here because a child path is always
strictly longer than the root it was joined from.

`ScanWith(dir, opts)` → `scan_with(dir, &opts)` (borrowed: the caller goes
on to pass the same `Opts` to `dir`/`objects`). It resolves `jobs` via
`Opts::jobs` (0 ⇒ available parallelism) exactly as `ScanWith` uses
`opts.jobs()`, while `scan` keeps clamping `< 1` to 1 like `Scan`.
`exclude_test.go` is ported one-to-one into `tests.rs`. The CLI example
gained no flag (Go's `cmd/amber-store` did not either). Go pin `c628b89`
(tag `v0.0.8`).
