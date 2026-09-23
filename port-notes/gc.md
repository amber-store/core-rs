# gc — port notes

Port of Go `gc/` at HEAD `a2ff135` (`gc.go`, `collector.go`, `cycle.go`,
`status.go` → `src/gc/{mod,collector,cycle,status}.rs`, tests →
`src/gc/tests.rs`). The obsolete simple-gc design (closure/dir/lease/union
files) does not exist at HEAD and was not ported.

## Go context/goroutine mapping

- **`Run(ctx, garbage)` → `Collector::run(&self, garbage: f64) ->
  Result<CycleStats, Error>`.** The context maps to a per-cycle
  `Arc<AtomicBool>` published under `mu` (`MuState::cancel_cycle`), tripped
  by `close`/`wipe`. `mark_from` checks it every 1024 pops **including
  n = 0**; the sweep entry re-checks it under the exclusive `ref_lock`. A
  cancelled cycle returns the typed `Error::Canceled` (message
  `"gc: cycle canceled"`; Go surfaces `context.Canceled`, i.e.
  `"context canceled"` — deviation, only observable via
  `Status.last_error` after a close/wipe race). `Error::is_canceled()`
  matches it.
- **`cycleMu` → `Mutex<()>` + `try_lock`**; contention returns
  `Error::CycleRunning` (`"gc: a cycle is already running"`,
  `is_cycle_running()`), never blocks, and is returned **before** `last`/
  `last_err` are recorded — exactly like Go, so an overlap never clobbers
  the last-cycle report.
- **`refLock` → `RwLock<()>`.** `prepare_ref` returns a guard-carrying
  `PreparedRef<'_>` whose `commit(self)`/`abort(self)` both just release the
  read guard — the analog of Go's single `sync.Once`-guarded closure;
  consuming `self` gives the exactly-once guarantee, and **dropping the
  handle without calling either is an abort** (Go has no such path; RAII
  makes it safe). The guard is held from the completeness walk through
  commit/abort; the walked closure goes to `observe_keys` before
  `prepare_ref` returns; the walk error wraps as
  `"gc: walking root {root}: {err}"` (`Error::Walk`, the caller's 404 —
  the inner `fstree::WalkError::Missing` is reachable via `source`).
  `release_ref(root)` is a no-op kept for protocol parity.
- **Background loop** (`interval > 0`): a `std::thread` + an
  `mpsc::channel<()>` stop channel; `recv_timeout(interval)` stands in for
  `time.Ticker` + select, so intervals separate cycle **ends** rather than
  ticking on a fixed cadence during long cycles (Go's ticker drops missed
  ticks; drift only, no behavioral difference for sane intervals). Loop
  cycles run `core.run(-1.0, …)`; errors land in `Status.last_error` only.
- **Close ordering deviation (deliberate):** Go's `stop()` cancels the loop
  context, which the *running background cycle's* context derives from, so
  cancellation reaches it before `<-done`. The port reproduces that context
  tree with `Core::loop_cancel: AtomicBool` (checked as a "parent" flag by
  background cycles only) — `close()` sets it and trips the per-cycle flag
  **before** joining the loop thread, otherwise the join could wait out a
  full uncancelled cycle. After the join: cancel again (foreground cycles),
  then the `cycle_mu` lock/unlock barrier. `close` is idempotent and always
  `Ok` (Go returns nil).
- **`midMark` hook**: `MuState::mid_mark: Option<Arc<dyn Fn() + Send +
  Sync>>`, set directly by in-module tests (the field is `pub(super)`,
  mirroring Go's package-private access). `Arc` rather than the suggested
  `Box` so the cycle can copy the value out under `mu` and call it with no
  lock held — exactly Go's read-then-call — without a take/put-back dance.
  Called after `mark_live` returns, **before** its error check. The test
  closure calls back into the collector through
  `Collector::test_handle()` (cfg(test)), a second façade over the same
  `Arc<Core>`; the test clears the hook afterwards to break the resulting
  Arc cycle — and must drop the taken hook *outside* the `mu` lock, since
  the handle's `Drop` closes the collector and `close` takes `mu`
  (assigning `mid_mark = None` under the guard self-deadlocks; found the
  hard way).
- **`last`/`lastErr`** under `mu`; recorded on success **and** failure,
  before `cycle_mu` is released. `lastErr` is stored as `Option<String>`
  (only its `Error()` string is ever read back, in `Status`); `run` still
  returns the typed error to its caller.
- `Collector` implements `Drop { close() }` (packstore precedent) so a
  dropped collector cannot leak the loop thread. Go relies on explicit
  `Close`; that remains the contract.

## Ownership (for wave 3 / CLI+bench)

`Collector::open(dir, objects: Arc<packstore::Store>, refs:
Arc<refstore::Store>, opts)` — **Arcs, not borrows**, because the
background loop thread must own the stores past any caller borrow
(`&'a`-based designs cannot spawn a non-scoped thread). For the CLI:

```rust
let objects = Arc::new(packstore::Store::open_with(dir.join("packstore"), …)?);
let refs = Arc::new(refstore::Store::open(dir.join("refs"), true)?);
let coll = gc::Collector::open(dir.join("closures"), Arc::clone(&objects), Arc::clone(&refs), opts)?;
```

Close order stays coll → refs → objects: `coll.close()` (and drop it) first;
the collector's Arcs die with it, so the CLI's `drop(refs)` /
`objects.close()` behave as today. `packstore::Store::close` takes `&self`,
so it is callable through the Arc. Note `run`/`status`/`why` etc. all take
`&self`; `Collector` is `Sync`.

## Cycle order (pinned, = Go)

threshold (`garbage >= 0` forces; else `opts.garbage`, downgraded to 0.1
iff `free_below(closures dir, min_free)`) → exclusive `ref_lock` {
`begin_barrier`; `roots()` } → roots error ⇒ `abort_barrier` + return →
`mark_live` with no lock → `mid_mark` hook → mark error ⇒ `abort_barrier`
+ return → `marked`, `mark_duration` (includes the snapshot) → exclusive
`ref_lock` { cancel check ⇒ `abort_barrier` + return; `CompactOpts {
min_dead_ratio: threshold, horizon: Some(now − grace), pace: throttle if
rate > 0 }`; `compact(|k| live.contains(k), …)` timed as `sweep_duration`;
map stats }. Compact consumes the grey set — no `abort_barrier` on that
path.

## Deviations from Go (beyond the concurrency mapping)

1. **`CycleStats`/`Status` counters are unsigned** (`u64`/`usize`) where Go
   uses `int`/`int64` (`CopiedBytes`, `FreedBytes`, `Body`, `Live`,
   `LiveBytes`, `GarbageBytes`); none can be negative. `Status.last_error`
   is `Option<String>` (Go: `""` = none). `start`/`sealed` are
   `SystemTime`; durations are measured on `Instant`.
2. **Compact stats on a failed sweep are lost**: Go maps the partial
   `CompactStats` even when `Compact` errors; the Rust
   `packstore::Store::compact` returns `Result<CompactStats, _>` (wave-1
   orchestrator API), so a failed cycle's sweep counters stay zero in
   `last`. Flagged in `port-notes/packstore-gc.md` too.
3. **`free_below` uses `libc::statfs`** (identical field mapping to Go's
   `unix.Statfs`: `f_bavail`/`f_bsize`/`f_blocks`) on both apple and linux
   targets. POSIX `statvfs` was rejected: Darwin's `statvfs` `fsblkcnt_t`
   is 32-bit and truncates block counts on large volumes. statfs failure —
   including a path no `CString` can hold — reports no pressure, `min == 0`
   means 5 % of the filesystem, and the multiplications are saturating.
   The probe path is the **closures dir**, as in Go.
4. **`throttle`** has no mutex (Go's guards a theoretically-shared `pace`;
   the Rust pace callback is a `FnMut` owned exclusively by compact's
   single append loop). `throttle_owed` keeps Go's divide-before-multiply
   shape; pinned by a Rust-only test with an i64-overflowing input.
5. **`horizon`/`eligible` underflow**: Go's `time.Now().Add(-grace)` cannot
   fail; `SystemTime::checked_sub` can on absurd `grace` values — the port
   substitutes `UNIX_EPOCH` (nothing real is eligible), and `Status` uses
   the epoch for Go's zero `time.Time` when a segment disappears between
   the `liveness` and `segments` listings.
6. **`Options.grace`/`interval` are `Duration`** (unsigned): Go's negative
   `Grace` (→ default) and negative `Interval` (→ no loop) are
   unrepresentable; zero means the same thing in both ports. `rate` stays
   `i64` with `<= 0` = unlimited, exactly Go.
7. **`Open`'s sweep** maps Go's `os.RemoveAll` per entry onto
   `remove_dir_all`/`remove_file` by file type; a `file_type` failure is
   classified as the sweep error. `os.ReadDir` errors return unwrapped
   (`Error::Io`, transparent display), as in Go.
8. **Error style**: one `thiserror` enum (`gc::Error`), one variant per Go
   wrap site/sentinel; `Objects`/`Refs`/`Children` are `transparent` for
   errors Go returns unwrapped; `Reference { name, source: Box<dyn Error +
   Send + Sync> }` covers the three `"gc: reference %q: %w"` sites (decode,
   key parse, `why`'s walk). Go's `%q` ↦ `{:?}` on the name (escaping
   differs for exotic names only). `Walk.source` is boxed (clippy
   `result_large_err`: the inline `WalkError` pushed the enum past 144
   bytes).

## Tests

All of `collector_test.go`, `cycle_test.go`, `oracle_test.go` are ported
into `src/gc/tests.rs` (flat `#[test]`s, `// ---` dividers naming the Go
files, snake_case descriptive names). Helpers: `TestStore`/
`new_test_store` (packstore default sync=true as in Go), `open_collector`,
`store_tree`, `put_test_ref`/`rm_test_ref`, `backdate_packs` (mtimes −2 h
via `std::fs::File::set_modified` on a write-opened handle — no new
crates, no unsafe).

- **Go PCG**: `Pcg` in tests.rs ports Go 1.26 `math/rand/v2`'s 128-bit LCG
  advance + DXSM-on-new-state, `uint64n` (masked power-of-two draw /
  Lemire) and `IntN`, plus a bitwise CRC-32/IEEE (the `crc32c` crate is
  Castagnoli-only). `pcg_matches_go_reference_draws` pins the port against
  draws generated from Go 1.26.5 with the suite's exact seeds
  (`NewPCG(7,11)` Uint64/IntN(3)/IntN(20); `NewPCG(crc32("keep"), 0)`
  UintN(256)). **Wave 3's bench will port its own copy** — unify into a
  shared test/bench helper later if desired.
- Rust-only additions: the PCG pin above, `throttle_owed` overflow pin,
  `free_below` smoke (statfs actually runs; missing path → no pressure),
  and an error-text assertion (`"gc: walking root "`) in
  `prepare_ref_missing_object_fails`.
- `run_overlap_refused` holds `c.core.cycle_mu` directly (fields are
  `pub(super)` for exactly this — Go's test grabs `c.cycleMu` the same
  way).

## For wave 3 (CLI/bench)

- Reference PUT protocol: read old → `prepare_ref(root)?` → `refs.put` →
  `commit()` (on put failure `abort()`) → `release_ref(old)` on overwrite
  (including old == new). Ref rm: resolve old root, `refs.delete`,
  `release_ref(root)`. `put_test_ref` in tests.rs is the reference shape.
- `run(-1.0)` = policy (CLI `--garbage` default −1); `>= 0` forces the
  line.
- `status()` runs a **fresh full mark** with no quiesce — it fails on a
  store whose references reach missing objects, and skips the active
  segment's liveness row. `why(k)` returns sorted names.
- Error texts the CLI prints are byte-compatible with Go except
  `Error::Canceled` (above) and `%q`-vs-`{:?}` escaping on exotic
  reference names.
- No gaps found in the wave-1 packstore GC surface or the fstree
  `check_complete` (visited-keys) API — everything needed existed with the
  documented signatures.

## Go PR #3 backport (2026-08-27)

`Collector::wipe` now takes the store reset as a callback and runs it while
holding the cycle slot (after cancelling and waiting out a running cycle),
because the mark reads segment mmaps unpinned. Go's `Wipe(reset func()
error) error` returns the reset error directly; the Rust signature is
generic over the callback's error type (`fn wipe<E>(&self, reset: impl
FnOnce() -> Result<(), E>) -> Result<(), E>`) since the collector itself
has nothing to fail with there. Nothing in this crate calls it (no server),
same as Go.

## Go PR #12 backport (2026-09-23)

No production change: the mark, `prepare_ref` and `why` dispatch through
`fstree::child_keys`, which now follows a commit to its tree and parents.
`commit_history_stays_live` and `prepare_ref_missing_ancestor_fails` port Go
`gc/commit_test.go`. One cost carries over from Go unchanged: `prepare_ref`
re-walks the closure on every reference put, and with commits the closure is
all of history.

## Go PR #14 backport (2026-09-23): the collector across processes

The cycle holds the packstore's gate from under its first reference lock to
under its last; `begin_span` / `Span::prepare_ref` bracket object writes and
the reference put that names them, reference lock first; `prepare_ref` and
`begin_write` (Go PR #8, unported until now) are built on it; `status`
refreshes the view first. Details and deviations:
`port-notes/packstore-multi.md`, section "gc". Tests: `gc/multi_tests.rs`
(Go `multi_test.go`, `span_test.go`, `process_test.go`); the second process
is the test binary re-executed with `--exact gc::multi_tests::child_process_entry`.
`collect_while_another_store_ingests` brackets with the collector's span where
Go's test opens the span on the store: the pattern the documents recommend.
