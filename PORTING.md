# Porting contract (Go → Rust)

This crate is a port of `github.com/jobs-build/amber-store-core` (Go), pinned
at commit `9f3c60ea3306f1b53509006dc45048878a75fd2f` (the merge of PR #4,
the pre-encoded record write path and `Reader.Records`). The Go sources
are the normative reference wherever this document or `architecture/` is
silent; a local checkout lives at `/Users/dragan/jobs-build/amber-store-core`.

## Compatibility contract

**Byte-identical** (same input ⇒ same bytes, enforced by golden vectors):

- 32-byte keys, BLAKE3 hashing, and every serialized object
  (`Blob`/`FileNode`/`DirLeaf`/`DirNode`/`XattrSet`) — hence identical root
  keys for identical logical trees.
- UltraCDC and item-chunker cut points.
- Reference records (canonical CBOR).
- Binary fuse filter sections (the Go construction is deterministic:
  `rngcounter` starts at 1; port it exactly).
- Segment footers (index + filter + trailer) given identical record bytes.
- Record **headers** and raw (uncompressed) records; wire-pack framing.
- PAX tar export.

**Interoperable but not byte-identical** (each side reads the other's output):

- zstd-compressed record payloads: Go uses `klauspost/compress` (default
  level, EncodeAll), Rust uses libzstd (`zstd` crate, default level 3). The
  compress-only-if-strictly-smaller rule is identical, but compressed frames —
  and therefore record CRCs, segment bodies, and pack bytes containing them —
  differ between implementations. Correctness is unaffected: keys hash the
  *uncompressed* object bytes.
- Segment *files* additionally depend on write order (Go's parallel writer is
  scheduling-dependent), so they are not reproducible run-to-run even in Go.

**Different, by design** (documented for the user's decision):

- `refstore`: Go uses Pebble; there is no Rust Pebble. This port uses **redb**
  with identical semantics (name → canonical record bytes verbatim, blind
  overwrite, lexicographic iteration, sync flag). A `refs/` directory written
  by one implementation is not openable by the other.

## Dependency mapping

| Go | Rust |
|----|------|
| `zeebo/blake3` | `blake3` |
| `hash/crc32` Castagnoli | `crc32c` |
| `klauspost/compress/zstd` | `zstd` (libzstd) |
| `FastFilter/xorfilter` BinaryFuse[uint16] | ported in `src/binaryfuse.rs` |
| `PlakarKorp/go-cdc-chunkers` ultracdc | ported in `src/chunkers.rs` |
| `fxamacker/cbor` (core deterministic) | hand-rolled in `src/cbor.rs` |
| `cockroachdb/pebble` | `redb` (semantic port only) |
| `archive/tar` (PAX write subset) | ported in `src/tarexport.rs` |
| `unix.Mmap` | `memmap2` |
| xattr syscalls | `xattr` crate |

## Rules for every module

- Port semantics **exactly**, including edge cases and validation order, from
  the Go file(s) named in the module's section. Read the Go tests too — port
  the interesting ones.
- Errors: corrupt-data conditions must be typed so callers can match them
  (mirror `ErrCorrupt`/`ErrMalformed` sentinels with error enums + `is_*`
  helpers). Never panic on untrusted input; no `unwrap`/`expect` on data
  paths. Include the same diagnostic detail Go includes.
- Public API: Rust-idiomatic equivalents (`Result`, iterators/closures for
  `Emit`/`Getter`), with doc comments carrying over the Go doc comments'
  content. Getter = `FnMut(Key) -> Result<Vec<u8>, E>` style generics or
  `&mut dyn` — pick per module, stay consistent with what fstree defines.
- Tests: unit tests in the module (`#[cfg(test)]`), golden-vector integration
  tests under `tests/` reading `tests/golden/` per `VECTORS.md`. Golden tests
  must **fail** (not skip) if the vector files are missing, except while the
  generator does not exist yet.
- `cargo fmt` clean; `cargo clippy --all-targets -- -D warnings` clean;
  `unsafe` only where unavoidable (mmap) with a `// SAFETY:` comment.
- Do **not** run `git commit`, edit `Cargo.toml`, `src/lib.rs`, or another
  module's files. If you believe a shared file must change, write the reason
  to `port-notes/<module>.md` instead and adapt locally.
- Record anything surprising (Go quirks ported, deviations, TODOs) in
  `port-notes/<module>.md`.

## Module notes

### `key` (Go: `key/`)

Exact algorithm per `architecture/keys.md`. `Key` is `[u8; 32]`, `Copy`,
`Ord`; hex `Display`. Canonical-length validation: first length byte non-zero
unless the length is the single `0x00` byte. `new_from_hash` truncates the
32-byte BLAKE3 digest to `32 - 1 - length_size`.

### `cbor` (Go: `cborx/`)

Canonical heads (shortest form) on **encode**; byte strings; and the xattr
map codec with keys sorted by their **encoded** bytes. Also expose the
primitive `append_head`/`read_head` helpers for `fstree`/`reference` to
reuse. Decode matches Go's actual behavior (not its doc comment): `readHead`
accepts **all five definite-length head forms**, including non-shortest ones,
and rejects only additional-info 28–31; trailing bytes are rejected. Do not
"fix" this laxness — read-compatibility with Go depends on it.

### `chunkers` (Go: `chunkers/` + vendored ultracdc)

Port `UltraCDC.Algorithm` verbatim (maskS=0x2F, maskL=0x2C, LEST=64, 8-byte
windows, hamming-to-0xAA table) **and** the driver loop from the upstream
`Chunker.Next`: window = up to MaxSize bytes buffered from the reader; the
final short chunk behavior and empty-input behavior must match
`chunkers.SplitBytes` (empty reader ⇒ zero chunks). Options default
2048/10240/65536; validate exactly as upstream (`64 ≤ … ≤ 1 GiB`, min <
normal < max). Item chunker: BLAKE3 of the item encoding, low `bits` bits of
the **little-endian u64 of the first 8 digest bytes**; `MinRun =
max(2^bits/4, 2)`, `MaxRun = 2^bits * 4`; `bits = 0` ⇒ mask 0 (every item ≥
MinRun is a boundary). Keep the upstream ISC copyright notice on the ported
ultracdc code.

### `binaryfuse` (Go: `FastFilter/xorfilter@v0.5.1` `binaryfusefilter.go` + `xorfilter.go`)

Port `BinaryFuse[uint16]` construction and `Contains` bit-for-bit:
`splitmix64`, `mixsplit`/`murmur64`, `fingerprint`, segment-length and
size-factor formulas, the `iterations % 4` segment-resize dance, duplicate
pruning, `MaxIterations`. The construction seed sequence is deterministic
(`rngcounter = 1`). **Float caution:** `calculateSegmentLength` /
`calculateSizeFactor` use Go's `math.Log` (portable FDLIBM). Port Go's
`math.Log` implementation into this module (private fn) rather than calling
`f64::ln`, so results are bit-identical on every platform; same for
`math.Round` semantics (half away from zero — use a manual impl, not
`f64::round`, and match Go exactly).

### `amberignore` (Go: `amberignore/`)

Port the matcher exactly (pattern parsing, `**`, negation, dir-only,
anchoring, last-match-wins, subtree scoping and composition). The Go tests
define the semantics; port them.

### `fstree` (Go: `fstree/`)

`Entry` with fields per `architecture/fstree.md`; encoding must byte-match
fxamacker core-deterministic output with `NilContainerAsEmpty`: integer map
keys 0–9 ascending; optional fields (5–9) omitted when empty; required keys
0–4 always present (uint for 0–3 with name as byte string, key 4 signed —
note CBOR negative-int encoding for negative mtimes); empty entry array
encodes as `0x80`. `XattrsIn` is a pre-encoded raw CBOR map spliced verbatim.
Decode (`DecodeFileNode`/`DecodeDirLeaf`/`DecodeDirNode`) must mirror Go's
acceptance behavior — read `decode.go` closely and match its strictness (and
its laxness) exactly; port `decode_test.go`. Builders: `DirBuilder`,
`IndexBuilder` (file + dir variants), children-before-parents emit order,
length-field arithmetic per `architecture/types.md`. Read paths: `ChildKeys`,
`ResolvePath`/`ResolveEntry` (path splitting semantics from `collect.go`),
`CollectEntries`, `LookupEntry` (DirNode binary search), `ListEntries`
(after/limit pagination), `WriteContent`, `ReachableKeys`, `CheckComplete`
(bounded-parallel walk; a sequential or scoped-thread implementation is fine
if observable behavior matches). `check_complete` returns the visited keys
(root first, BFS discovery order, each once; `Err` returns no partial list) —
the collector hands them to the write barrier.

### `amberpack` (Go: `amberpack/`)

Record codec per `architecture/amberpack.md`: 46-byte header, CRC-32C over
the record with the CRC field zeroed, compress-only-if-strictly-smaller
(libzstd default level), parse validations in Go's order with equivalent
error classification (`Corrupt` vs `Malformed`), 256 MiB `slen` cap on the
stream reader, magic `AMBERPK\x03`, `tagEnd = 0x00`, explicit rejection of
versions 1 and 2. Writer streams records then the end marker; reader is an
iterator that validates each record fully (including key canonicality) but
not payload hashes, or (`records`) hands each validated record over
undecoded with its parsed header — the read-side counterpart of
`add_record`.

### `packstore` (Go: `packstore/`, all files)

Full port: store open/scan (segment file naming from `packstore.go`), active
segment append + recovery tail-scan (`recover.go`), sealing with footer
(`footer.go` — already-specified layouts; fanout on the **last** key byte),
sealed-segment mmap reads (`memmap2`, bounds-checked, no CRC on hot path),
`has`/`get`/`getRecord`/`storedSize`/`locate`, options (`WithSegmentSize`,
`WithSync`), `missing.go` (filter-then-index), `verify.go` (scrub),
`parallel.go` (bounded worker pool, `seenSet` dedup, BLAKE3 verification
before commit, stats), and `prepare.go` (an `Object` offered as a
pre-encoded `record` instead of `data`: parsed, key-checked, appended
verbatim, decoded and rehashed under `verify`; `append_record` shares the
check). Match fsync/rename durability discipline. Concurrency:
scoped threads + channels; observable semantics (dedup, stats, error-stops)
must match Go.

GC surface (Go: `markset.go`, `barrier.go`, `gc.go`, `compact.go` →
`markset.rs`, `barrier.rs`, `gc.rs`, `compact.rs`): the mark-set bitmaps over
footer index positions, the write barrier's grey capture (observe *before*
the dedup `has` — dedup hits must grey), segment listing/scan/record/
re-append/remove, and `compact` (seal, strict-mtime-before-horizon victim
selection, parallel re-verify + single appender under the append lock,
unlink only after durable copies). Deviations from Go's scrub-wait and
write-token machinery are documented in `port-notes/packstore-gc.md` —
Rust's `Arc`-held mmaps make Go's munmap-wait unnecessary.

### `refstore` (Go: `refstore/`)

Semantic port on redb (see contract above). Same validation and API shape:
put/get/delete/list (lexicographic), records stored verbatim, `sync` flag
honored (redb durability settings). Read `refstore.go` for exact behaviors
(missing-name errors, empty-store list, etc.).

### `gc` (Go: `gc/`)

The mark-and-sweep collector of `architecture/mark-sweep-gc.md`: mark from
the references' roots into a `packstore` mark set, sweep via `compact`.
Port `gc.go`/`collector.go`/`cycle.go`/`status.go` exactly: the cycle's
lock/barrier order (barrier on, then the roots snapshot, both under the
exclusive reference lock; `abort_barrier` on every early exit; the sweep
again under the exclusive lock), `prepare_ref`'s guard held from the
completeness walk to commit/abort, the no-op `release_ref` kept for
protocol parity, policy thresholds (0.5, or 0.1 under min-free pressure
probed at the closures dir), and the loud mark abort on a missing object.
Go's contexts/goroutines map to cancel flags + threads; see
`port-notes/gc.md`.

### `reference` (Go: `reference/`)

Canonical record codec (keys 0–5), `ValidateName`/`ValidateUser` rules and
bounds (1–1024 bytes UTF-8, `@`/control-char rules, 64 KiB signature cap),
and Decode's canonical-bytes enforcement — match `reference.go` exactly,
including whether Decode re-encodes-and-compares or validates structurally.

### `inbox` (Go: `inbox/`)

Durable receiving: entry file layout and Meta header codec (`entry.go`),
fsync/rename discipline, drain worker draining packs into a packstore via the
parallel writer, crash-recovery on open. Port `slog` usage to a minimal
logging callback or `log` facade (document choice in port-notes).

### `ingest` (Go: `ingest/`)

`Objects`/`Dir`/`Scan` APIs, options (jobs, chunk opts, xattr inline max,
no-ignore), scan order (bytewise-sorted dirents), metadata capture (lstat:
mode/uid/gid/mtime ns; macOS + Linux xattr via the `xattr` crate matching
`xattr_darwin.go`/`xattr_linux.go` behavior incl. error tolerance),
`.amberignore` loading/composition/pruning with always-store-the-ignore-file
rule, single-file ingest, parallel file chunking (`parallel.go`) with
deterministic output object stream (verify what Go guarantees and match it),
progress stats. The golden root-key test (build the VECTORS.md tree from a
materialized directory where possible) plus interop tests gate this module.

### `tarexport` / `tarextract` (Go: `tarexport/`, `tarextract/`)

Export: port the **PAX write subset of Go's `archive/tar`** so output is
byte-identical: ustar field fitting, when PAX records are emitted (mtime
with nanoseconds or out-of-range fields, long names/linknames, large ids),
record formatting (`"%d key=value\n"` self-including length), record-key
sorting, `PaxHeaders.0/<name>` extended-header naming and its header fields
(incl. mode/mtime of the extended header itself), `SCHILY.xattr.*`, dir
trailing slash, `mode & 0o7777`, devmajor/devminor, socket skipped, 512-byte
padding and the two-zero-block terminator. The golden `tar_go.tar` must
byte-match. Extract: PAX reader (hand-rolled or `tar` crate — must handle ns
mtimes, xattrs, long names, devices), restore metadata with the same
best-effort policy as Go (`tarextract.go`: ownership/xattr error handling,
dir mtimes applied after children, path-safety checks).

### CLI example (`examples/amber-store.rs`)

Dev-only mirror of `cmd/amber-store` (ingest/ls/export/restore/ref/gc,
--store, --segment-size, ref:NAME[@PATH] addressing) for interop testing;
uses only the public crate API + clap. No progress UI needed. The gc
subcommands' output format strings are byte-compatible with Go (the bench
and tests parse them); reference writes route through the collector.

### bench example (`examples/amber-bench.rs`)

Port of `cmd/amber-bench`, the ingest → delete → gc benchmark. The dataset
generator reproduces Go's byte streams exactly (Go `math/rand/v2` PCG +
`IntN`/`Shuffle`, xorshift64* file content) so both implementations ingest
the identical dataset; results.json is schema-compatible with Go's. See
`port-notes/amber-bench.md`.

### Verified repair and reference batches

`packstore::Store::put_verified` matches Go `packstore.Store.PutVerified`.
It verifies supplied content before mutation and checks every indexed copy.
A healthy newer copy does not hide damage in older segments.
Replacement preserves segment IDs, index key order, and unrelated record bytes.
Existing readers retain their old mapping until they release it.
New readers use the repaired mapping after publication.
New writes and healthy deduplication follow the configured sync option.
Replacement files and directory renames always sync.
Repair does not rebuild corrupt footers or discover unindexed objects.
An error can leave some copies repaired. Retrying is safe.
Recovery accepts a complete footer before scanning an active file's body.
This preserves later records after payload damage during an interrupted seal.

