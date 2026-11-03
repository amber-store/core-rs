# packstore GC surface (Go pin `e4fcb60` → HEAD `a2ff135`)

Ported: `packstore/markset.go`, `barrier.go`, `gc.go`, `compact.go` (new
files `markset.rs`, `barrier.rs`, `gc.rs`, `compact.rs`), plus the pin→HEAD
deltas of `packstore.go` (append split, write tokens, barrier hooks, store
fields), `footer.go` (`searchIndexPos`/`lookupPos`), `parallel.go` (token +
observe hook), and the `footer_test.go` "segCount zero" subtest. Tests live
in `src/packstore/gc_tests.rs` (markset_test.go, gc_test.go, compact_test.go,
compact_concurrent_test.go), with the filter-geometry delta added to
`store_tests.rs`.

## Sanctioned deviations (orchestrator-approved)

1. **No scrub condvar machinery.** Go replaced its scrub `WaitGroup` with a
   `scrubMu`/`scrubN`/`scrubC` cond var so `Remove`/`Wipe` can wait out
   in-flight mmap walks before munmap (a Go munmap under a walker is an
   uncatchable SIGSEGV). The Rust port's scrubs/readers hold
   `Arc<SealedSegment>` clones, so the mmap lives until the last handle
   drops; `remove`/`remove_victims` detach under the write lock and unlink
   with no wait (same pattern as the pre-existing `verify`/`wipe`/`close`).
   Consequences:
   - `TestRemoveWaitsForScrubs` cannot be ported literally (there is nothing
     to wait for). Its observable contract is ported as
     `remove_during_scan_index_is_safe`: a `remove` landing mid-`scan_index`
     returns immediately, the scan still completes over the full index, and
     the file is unlinked. `TestRemoveDuringReads` ports directly
     (`remove_during_reads`).
   - Go's `Remove` aggregates a firstErr across `seg.close()` (munmap + fd
     close), unlink, and dir fsync. The Rust munmap happens on Arc drop and
     cannot report an error; firstErr covers unlink + dir fsync.
   - `Verify` needed no change (it never used the Go scrub counter here; its
     snapshot already owns its mappings).
2. **Write tokens are counter ids.** Go keys the inflight map by
   `*writeToken` pointer; Rust uses a `u64` counter → `Instant` map under one
   mutex (`gc::Writes`), with an RAII `WriteToken` whose `Drop` is Go's
   deferred `endWrite` (covers every return path).
   `oldest_inflight_write() -> Option<Instant>` returns a **monotonic
   `Instant`**, not Go's wall-clock `time.Time`: the ported test asserts
   `before <= start <= now` ordering, which `Instant` guarantees and
   `SystemTime` does not (it can step backwards). Wave 2's gc module should
   compare against `Instant`s if it ever consumes this (the mark-sweep
   collector does not; the API exists for parity + tests).
3. **`allEntries` iterator.** Go's `iter.Seq[indexEntry]` is a plain
   `fn all_entries(&SealedSegment) -> impl Iterator<Item = IndexEntry>` in
   compact.rs (private).
4. **Pipeline cancellation.** `copyLive`'s `context.WithCancel` maps to an
   `AtomicBool` cancel + first-error slot (`Pipe`, same shape as
   parallel.rs's `Run`) with `sync_channel`s. Channel-closure does the rest:
   the producer drops the `cands` sender when done/cancelled; verifiers exit
   on channel close and drop their `verified` sender clones; the appender
   (calling thread, holding the append lock) drains `verified` until close.
   On verify failure a worker records the error and **keeps draining** so
   the producer can never block on a full channel (Go unblocks its producer
   via `ctx.Done` instead — same net behavior).

## Other intentional differences

- **`SegmentInfo.body` is `u64`** (Go: `int64`). `parse_footer` guarantees
  `body_len >= len(MAGIC_HEADER)`, so the subtraction cannot underflow. The
  orchestrator's API note allowed either; wave 2's Status math
  (`Body = Live + DeadBytes` as int64) should just use u64/i64 as it sees
  fit.
- **`CompactOpts.horizon` is `Option<SystemTime>`** (Go zero-`Horizon` =
  always-eligible maps to `None`). Comparison is strict: eligible iff file
  mtime `< horizon`. `pace` is `Option<Box<dyn FnMut(usize) + Send>>`.
- **`compact` returns `Result<CompactStats, Error>`**, so partial stats are
  lost on error. Go returns `(stats, err)` and its gc `cycle` copies the
  partial stats into `CycleStats` **even when Compact errors**. Wave 2: if
  `last`-cycle stats on a failed sweep must match Go, either live with
  zeroed compact fields on error or ask for a `(stats, Result)` tuple like
  `write_parallel` — flag this when porting `cycle.go`.
- **Fresh-slice discipline is unnecessary here.** Go rebuilds `s.sealed` as
  a fresh slice because concurrent readers may hold the old backing array;
  Rust readers can only hold `Arc` clones (the `Vec` itself is only reachable
  under the `RwLock`), so `remove`/`remove_victims` mutate in place under the
  write lock. Same observable semantics.
- **`remove` holds the append lock through unlink + dir fsync.** Go unlocks
  both mutexes before `waitScrubs` (so a long wait cannot stall writers) and
  then uses its lock-free `dirF` field. Rust has no wait, and the directory
  handle lives inside the append state, so the guard is simply kept for the
  (short) unlink + fsync tail. The append→shared lock order is preserved.
- **Grey-set wrapper is unconditional.** Go wraps `live` only when the taken
  grey set is non-empty; the Rust closure checks an `Option<HashSet<Key>>`
  that greys nothing when `None`/empty — behaviorally identical.
- **`segCount == 0` filter geometry**: Go HEAD adds a dedicated *first* case
  in `parseFilterSection` ("filter segment count is zero"). The Rust
  `BinaryFuse16::parse_section` (binaryfuse.rs, not in this wave's scope)
  already rejected `seg_count == 0` — documented there as deliberately
  stricter than pre-GC Go. No code change was needed; the ported subtest in
  `store_tests::parse_filter_section_rejects_bad_geometry` pins it.
- **Atomic orderings**: `capturing` uses `SeqCst` (Go atomics are seq-cst);
  correctness only needs the mutex re-check, so this is belt-and-braces.
- `check_record`'s classification: parse/decode/key-mismatch failures →
  `Corrupt { verify: false }`; `verify_object` failures →
  `Corrupt { verify: true }`, matching Go where `verifyObject` wraps
  `ErrVerify` and `verifyRecord` adds `ErrCorrupt` (so `errors.Is` matches
  both).

## New/changed surface consumed by wave 2 (gc module)

```rust
// markset.rs
pub struct MarkSet;                       // holds Arc snapshots, NOT a Store borrow
impl Store { pub fn new_mark_set(&self) -> MarkSet }
impl MarkSet {
    pub fn mark(&mut self, k: Key) -> (bool, bool);   // (newly, present)
    pub fn contains(&self, k: Key) -> bool;
    pub fn marked(&self) -> usize;
}
// barrier.rs
impl Store {
    pub fn begin_barrier(&self);
    pub fn abort_barrier(&self);
    pub fn observe_keys(&self, keys: &[Key]);
    pub(crate) fn observe(&self, k: Key);
    pub(crate) fn take_grey(&self) -> Option<HashSet<Key>>;
}
// gc.rs
pub struct SegmentInfo { pub id: u64, pub sealed: SystemTime, pub body: u64, pub keys: u64 }
impl Store {
    pub fn oldest_inflight_write(&self) -> Option<Instant>;
    pub fn segments(&self) -> Result<Vec<SegmentInfo>, Error>;
    pub fn scan_index(&self, id: u64, f: impl FnMut(Key, u64, u32)) -> Result<(), Error>;
    pub fn record(&self, id: u64, off: u64) -> Result<Vec<u8>, Error>;
    pub fn has_outside(&self, id: u64, k: Key) -> Result<bool, Error>;
    pub fn append_record(&self, k: Key, raw: &[u8]) -> Result<(), Error>;
    pub fn sync(&self) -> Result<(), Error>;
    pub fn remove(&self, id: u64) -> Result<(), Error>;
}
// compact.rs
pub struct SegmentLiveness { pub id: u64, pub sealed: bool, pub live_keys: usize,
    pub dead_keys: usize, pub live_bytes: u64, pub dead_bytes: u64 }
pub struct CompactOpts { pub min_dead_ratio: f64, pub horizon: Option<SystemTime>,
    pub pace: Option<Box<dyn FnMut(usize) + Send>> }   // Default: 0.0 / None / None
pub struct CompactStats { pub segments_scanned: usize, pub segments_compacted: usize,
    pub victims: Vec<u64>, pub records_copied: usize, pub bytes_copied: u64, pub bytes_freed: u64 }
impl Store {
    pub fn liveness(&self, live: impl Fn(Key) -> bool) -> Result<Vec<SegmentLiveness>, Error>;
    pub fn compact<L: Fn(Key) -> bool + Sync>(&self, live: L, opts: CompactOpts)
        -> Result<CompactStats, Error>;
}
// mod.rs
Error::UnknownSegment  // "packstore: no such segment"
impl Error { pub fn is_unknown_segment(&self) -> bool }
// footer.rs (pub(crate)): search_index_pos, FooterView::lookup_pos
```

`MarkSet::contains` is `&self`, and `MarkSet` is `Sync`, so
`s.compact(|k| m.contains(k), opts)` works — the mark-set closure satisfies
`Fn(Key) -> bool + Sync`, and `compact`'s producer thread calls it from a
`thread::scope`.

## Notes for wave 2

- `Segments().sealed` comes from **file mtime**; gc's grace tests backdate
  packs with utimes/chtimes — anything that rewrites `.seg` files breaks
  that. `compact`'s horizon check re-stats the file per pass.
- `take_grey` is consumed **unconditionally at the top of `compact`** —
  before the closed/failed checks, even on zero victims — so `capturing` is
  never left true after a `compact` call. Every early exit *between*
  `begin_barrier` and `compact` in the gc cycle must call `abort_barrier`.
- `compact` seals the active segment itself; the gc module never seals.
- The append lock excludes object writers for the whole `compact`; the gc
  module's only quiesce duty is excluding reference publication (its
  refLock) around the sweep.
- `store_tests::sealed_store` was promoted to `pub(crate)` so gc_tests can
  reuse it (Go shares it via package scope).

## Go v0.12.0: what may be acknowledged, and a sweep past a bad record

Ports Go `amber-store/core` #27 and #29 (`packstore/{active,repair,compact,recover,footer,verify}.go`,
`gc/{collector,cycle}.go`, `cmd/amber-store/gc.go`) with their tests, in
`durability_tests.rs` and `compact_skip_tests.rs`. `architecture/` says what the
rules are. Where the port differs from Go:

- `Error::Skipped(Box<CompactStats>)` is Go's `(stats, *SkippedError)`: a `Result`
  carries one or the other, so the stats travel in the error, and
  `Error::compact_stats` reads them. Any other failure still loses them, as noted in
  `gc.md`.
- A crashed seal's name: Go keeps an immutable `path` and an atomic `renamed`, which
  a refresh sets. Here a refresh publishes the segment under its new name, and
  `SealedSegment::stat` falls back on it in between, so there is no flag to race on.
- `Store::fsync(&File, &Path)` takes the path because a Rust `File` has no name, and
  the test hook `before_fsync` is keyed on it. `copy_workers` pins the pass to one
  worker for the tests that need an order; Go's tests set `GOMAXPROCS`.
- `get_verified`'s failure is `Error::Corrupt { verify: true }`, Go's error wrapping
  both `ErrCorrupt` and `ErrVerify`.
