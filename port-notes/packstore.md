# packstore port notes

Port of Go `packstore/` (all files) to `src/packstore/`. The original porting
agent was killed mid-task; this note records the finishing pass: what was
missing, the deviations that stand, and the live cross-check against Go.

## State found at the finishing pass, and what was completed

The source port (`mod.rs`, `footer.rs`, `recover.rs`, `verify.rs`,
`missing.rs`, `parallel.rs`) was semantically complete — a function-by-function
audit against `packstore.go`/`segment.go`/`footer.go`/`recover.go`/
`verify.go`/`missing.go`/`parallel.go` found validation order, fsync/rename
discipline, tail-scan semantics, seal-on-threshold, close-does-not-seal, mmap
bounds checks, and error text all faithful. What was missing:

- **Test parity.** Only `packstore_test.go` + `oracle_test.go` had been ported.
  `footer_test.go`, `recover_test.go`, `verify_test.go`, `missing_test.go`
  (store-level), and `parallel_test.go` were unported — `testutil.rs` already
  carried their helpers (`test_entries`, `write_sealed_file`, `build_body`,
  `refresh_footer_crc`, `be_u64`), all dead code. All meaningful cases are now
  in `store_tests.rs` (43 additional tests: index/filter section round-trips
  and corruption rejection incl. the keyCount-wrap and crafted-geometry
  regressions, sealed-segment round trip and crafted-offset bounds checks,
  tail-scan truncation at every byte / no-resync / partial-footer / trailing
  garbage, scrub detection of body corruption / index lies / key-count
  mismatch / hash mismatch (corrupt AND verify classes), missing
  order+multiplicity, parallel-writer stats/dedup/verify/error-flush).
- **Mechanically-flattened let-chains.** The post-crash compile fix had
  denested the let-chains into nested `if`s (clippy `collapsible_if` under
  `-D warnings`); restored to edition-2024 let-chains. `cargo fmt` had also
  never been run on the unit.
- **`tests/golden_packstore.rs`** (VECTORS.md `segments_go`) did not exist.
  Now: copies the fixture to a tempdir, opens it, byte-compares all 31
  manifest objects, checks the 4 absent keys (has/get/missing), scrubs,
  resumes the Go-written active tail with a new append across a reopen, and
  separately rewrites the golden objects through `write_parallel` on a fresh
  16 KiB-segment store with rotation + reopen.
- **`SealedSegment`** gained a manual `Debug` impl (test ergonomics only).
- This file.

## Deviations from Go (all deliberate)

- **Error surface.** One `Error` enum with `is_not_found` / `is_closed` /
  `is_corrupt` / `is_verify` helpers standing in for `errors.Is`; `Context`
  wrapping preserves classification like `%w` chains. Corruption messages
  reproduce Go's full text including the `amberpack: corrupt pack data: `
  prefix (packstore's `ErrCorrupt` aliases amberpack's). Scrub hash findings
  set `Corrupt { verify: true }`, matching Go's double wrap of `ErrCorrupt`
  and `ErrVerify`.
- **fd/mmap close errors.** Go can surface `f.Close()` / `Munmap` errors from
  `sealActiveLocked`, `Wipe`, and `Close`; Rust closes fds and unmaps when the
  last `Arc` drops, which cannot report an error. The fsync errors — the ones
  that matter for durability — are surfaced identically.
- **No scrub WaitGroup.** Go's `Close`/`Wipe` block on `scrubs.Wait()` so
  munmap never runs under a live `Verify` walk. The Rust scrub snapshots
  `Arc<SealedSegment>`s, which own their mappings, so a concurrent
  `close`/`wipe` is memory-safe without waiting; `verify` still fails fast
  with `Closed` when the store is already closed. Covered by
  `verify_concurrent_with_close`.
- **Cancellation.** `Verify(ctx)` becomes `verify(cancel: impl Fn() -> bool)`,
  checked once per record like Go's `ctx.Err()`; returns `Error::Canceled`
  (Go returns `context.Canceled`).
- **`missing` worker plan.** Go spawns `min(GOMAXPROCS, ceil(n/64))`
  goroutines gated by an errgroup limit of 16; Rust clamps the worker count to
  16 directly and enlarges chunks. Output (order + multiplicity) is identical;
  the chunk-clamp regression is pinned by unit tests on `plan`/`chunk_bounds`.
- **`write_parallel` returns `(WriteStats, Result)`** so an erroring run still
  reports the work done, exactly like Go's `(WriteStats, error)`.
- **`Store` implements `Drop`** (best-effort `close()`); Go relies on explicit
  `Close`. Double-close returns `Ok` like Go.
- **`Options` builder** replaces the functional options; defaults identical
  (256 MiB segment size, sync on).
- **Filter parse strictness.** `binaryfuse::parse_section` additionally
  rejects `segment_count == 0`; Go-built filters always have ≥ 1, so accept
  behavior on real files is unchanged and the reject class (corrupt) matches.
- **Test RNG.** `testutil` streams use splitmix64 rather than Go's PCG; the
  ported tests rely on the properties (compressible/incompressible), not the
  bytes.

## Live Go cross-check (2026-08-08, throwaway harness, deleted after)

Harness: `/tmp` Go module with a `replace` to the pinned checkout; env-gated
temporary Rust tests (removed after the run).

- **Rust → Go**: `write_parallel` (verify on, 64 KiB segments) wrote the 31
  golden objects + 3 tail objects → 2 sealed segments + 1 unsealed active.
  Go `Open` tail-scanned the Rust active segment, byte-verified all 34
  objects (`Get`, `StoredSize`), confirmed 4 absent keys, and `Verify` —
  which re-derives the index section and compares it bytewise against the
  Rust-built footer — passed clean.
- **Go → Rust**: Go wrote a fresh 34-object store (different seeds than the
  committed fixture; 2 sealed + unsealed tail). Rust opened it, byte-verified
  every object, confirmed absent keys, `verify` clean.

No mismatches in either direction; no Rust-side defects surfaced by the
cross-check.

## Go PR #3 backport (2026-08-27)

- `seal_active` truncates the file to the logical size before writing the
  footer (`set_len(aw.size)`), since the footer parse anchors at EOF and a
  failed write can leave junk past it.
- `write_parallel` always fsyncs once at the end of the run (even
  dedup-only runs — the hits may match another run's unsynced appends, and
  a sync failure becomes the run's error if it had none); workers no longer
  issue an end-of-input flush, only the batch-size ones. `Store` grew the
  Go-matching `fsyncs` counter (test-observed, maintained unconditionally
  in `sync_active` like Go's).

## Go PR #4 backport (2026-09-06)

- `Object` gains `record: Option<Vec<u8>>` (Go: `Record []byte`), the
  Go-literal shape rather than a `data | record` enum: `Object { key, data }`
  is built in six places but `.data` is read in about eighty (tests
  included), and the Go tests port one-to-one. Rust cannot tell a nil `Data`
  from an empty one, so the "carries both Data and Record" rejection fires
  on a non-empty `data` next to a `record`; an empty `data` next to a
  `record` is taken as a record object (Go rejects `Data: []byte{}` there
  and accepts `nil`). The record's own key check makes the difference
  harmless. The error text keeps Go's field names (`carries both Data and
  Record`) for byte parity.
- `prepare.rs`: `prepare(obj, verify) -> (record, ulen)` is Go's `prepare`;
  its record half is split out as `check_record(k, raw, verify) -> Record`
  (parse, key equality, exact length, optional decode + rehash) so that
  `append_record`, which borrows its slice, shares the check without copying
  the record into an `Object`. Error mapping: `parse_record` /
  `decode_payload` failures pass through as `Error::Pack` (corrupt class,
  as Go returns the amberpack error unwrapped), key/length mismatches are
  `corrupt(...)`, a rehash failure is `Error::Verify`. `run_writer` and
  `write_batch` route through `prepare`; `bytes_stored` charges a record
  object's `ulen`, as in Go.
- Go's `AppendRecord` nil-raw guard is unrepresentable: a `&[u8]` is always
  offered as a record, so `append_record(k, &[])` fails in `parse_record`
  (corrupt class). The ported test pins the class and that nothing is
  stored.
- Tests: `record_tests.rs` (Go `record_test.go`, all eight cases).

## Go PR #12 backport (2026-09-23)

`verify_object` checks the length field of `Commit` keys too: like `Blob` and
`XattrSet`, a commit's length is its own serialized byte length. Port of Go
`TestVerifyObjectChecksCommitLength` in `verify.rs`.

## Go PR #14 backport (2026-09-23)

Any number of stores, in any number of processes, on one directory: sidecar
index, segment ownership, refreshing views, the `gc.lock` gate. Notes,
deviations and the live cross-check are in `port-notes/packstore-multi.md`.
Two statements above no longer hold and are kept for the record only: the
store is not single-owner any more, and `open` recovers nothing (recovery
moved to the writer that takes a segment), so "tail-scan at open" now reads
"at adoption".
