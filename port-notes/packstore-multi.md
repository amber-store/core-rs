# packstore, many processes (Go PR #14)

Port of Go PR #14: `sidecar.go`, `recover_sidecar.go`, `active.go`, `view.go`,
`gate.go` as `sidecar.rs`, `recover_sidecar.rs`, `active.rs`, `view.rs`,
`gate.rs`, plus the changes to `packstore.go`, `gc.go`, `compact.go`,
`repair.go`, `missing.go`, `parallel.go`, `markset.go`, `footer.go` and, in
`gc`, `collector.go`, `cycle.go`, `status.go`. The format and the protocol are
specified in `architecture/packstore.md`; its "Rules for implementations" are
what this port keeps to. Tests: `sidecar_tests.rs`, `multi_tests.rs`,
`gate_tests.rs`, and `gc/multi_tests.rs`.

## What is shared with Go, and how it was checked

- The sidecar file (`<id>.seg.active.idx`: magic `AMBERIX\x01`, 56-byte
  records, CRC-32C) and its reading rules; the ownership flock on the data
  file; the temporary name and the re-check after locking; `gc.lock` with its
  8-byte big-endian generation; the shared directory flock that keeps releases
  from before this change out.
- Checked live, both ways, with a throwaway harness (deleted): Go wrote an
  active segment and closed; Rust's `recover_from` accepted Go's sidecar whole
  (every entry, nothing missing, everything durable, no fall-back to a scan),
  adopted the segment and went on appending to data and sidecar; Go's
  `recoverFrom` then accepted the mixed sidecar whole and read every object;
  each side's sweep moved the generation the other then read.
  `interop/check.sh` cross-reads whole stores on top of that. A fall-back scan
  would hide a format mismatch from the script, hence the explicit check.

## Deviations from Go, all deliberate

- **Nothing is closed under a reader.** Go closes a dropped foreign view's
  file and unmaps retired segments, so it needs `retire`, the scrub counter
  and, in the refresh, tolerance of `os.ErrClosed`. Here sealed segments and
  foreign views live in `Arc`s: whoever still reads one keeps its mapping or
  its file. `retire`, `waitScrubs` and the closed-file case have no
  counterpart, and Go's `TestLookupsDuringTheFirstWriteSeeNoError` passes by
  construction (kept as a race test).
- **A mapped segment's path cannot change.** Go updates `path` in place when a
  crashed seal, mapped under its active name, is renamed by its adopter. Here
  the refresh maps the file again under its new name and lets the old mapping
  go; when the store changed the view meanwhile (`struct_epoch` moved) the
  old one stays until the next refresh, as any not-listed segment does.
- **A foreign view's index is behind its own lock.** `ForeignActive.state` is
  an `RwLock`: written under the store's shared lock held for writing, read
  under it by lookups, and read under the refresh lock alone by `poll`. Lock
  order: append, refresh, shared, then a view's state.
- **Positions are `i64` in recovery**, as in Go, and an entry's offset is read
  as Go reads it (`off_i64`, a wrapping `end`), so that every comparison on a
  damaged or crafted entry that passes its CRC comes out as it does there.
- **`poll` was always free of side effects here**: `advance` takes the scan
  position by value and returns the next one. Go got there through a review
  fix; the ported test (`a_failed_refresh_loses_nothing`) pins it.
- **Cancellation is a callback.** `begin_sweep(cancel)` stands for Go's
  context. Go wakes a waiter the moment its context ends; here waits look at
  the callback at least every 50 ms. New `Error::GateCanceled`; the collector
  maps it to its own `Error::Canceled`.
- **Guards instead of `done` functions**: `WriteSpan`, `Sweep`, and in `gc`
  `Span` and `WriteGate`. Dropping ends them. Go checks at run time that a
  span is not used after `End`; here the type system does (`end(self)`).
  Go's internal `beginWrite` is `begin_write_token`; the exported
  `BeginWrite` is `begin_write`.
- **Locks are let go of explicitly.** A segment's flock belongs to its file,
  which here closes with the last handle, possibly a reader's. `close` and
  `seal_idle` therefore unlock (`active::unlock`) instead of closing, and the
  gate's `close` unlocks `gc.lock`, whose file closes with the store.
- **The racy-timestamp window is per store** (an atomic), not a package
  variable: Rust runs a crate's tests on threads of one process.
- **Test hooks** are `Hooks { after_list, after_detach }` on the store and
  `never_refresh` in the gate, all unset outside tests.
- `write_parallel` returns zeroed stats next to a gate error (Go returns the
  zero `WriteStats`).
- `gate.looked` is carried over although, in both implementations, a store
  starts at generation 0 and any sweep leaves at least 1 in the file, so the
  first span of a store that opened during a sweep refreshes either way.

## gc

- The cycle takes `begin_sweep` under its first reference lock and drops the
  guard under its last (declared after the lock guard, so dropped before it).
  A failed or cancelled mark goes through the same exit.
- `Collector::begin_span`, `Span::prepare_ref`; `prepare_ref` is built on it
  and `PreparedRef` now owns the span it opened (or nothing, inside a caller's
  span). `Collector::begin_write` (Go PR #8), listed as unported until now,
  came with it. Still unported from that PR: `inbox.WithGate`. The store's own
  gate now keeps a sweep and an inbox drain apart (`compact` waits out and
  holds off this store's write spans), so nothing is lost without it; what is
  missing is only the collector's fairness to a drain.
- `status` refreshes the view before its advisory mark.

## Tests

Ported: all of `sidecar_test.go`, `recover_sidecar_test.go`, `multi_test.go`,
`gate_test.go`, `concurrent_view_test.go`, `durable_dedup_test.go`,
`process_test.go`. The second process is the test binary itself, re-executed
with `--exact packstore::multi_tests::child_process_entry` and
`PACKSTORE_TEST_CHILD_DIR` set (Go: `TestMain`). Gone with the single-owner
store: `second_open_fails`, `multiple_active_files_fail_open`;
`crash_between_footer_and_rename` follows Go's rewrite (a reader finishes
nobody's seal).

Mutants run once, each killed by the test named: no refresh lock across detach
and unlink (`a_lookup_between_detach_and_unlink…`); nested spans waiting for a
pending sweep (`writes_inside_a_span_pass_a_waiting_sweep`); a sweep that does
not move the generation (three generation tests); unsynced foreign records
counted as reliable (both durable-dedup tests); a refresh that never lists
again (`refresh_lists_again_when_a_listed_segment_vanished`).

The independent review ran 22 more and found six that the suite, green, let
through. Each now has a test, most of them the reviewer's reproducers: the
shared file lock converted in place, and a sweep's lock released under spans
that joined it (`a_waiting_local_sweep_leaves_the_shared_lock_alone`,
`the_end_of_a_sweep_waits_for_spans_that_ran_under_it`); `begin_span` taking
the gate before the reference lock, which deadlocks against a cycle
(`begin_span_takes_the_reference_lock_before_the_gate`); `write_parallel`'s
duplicate check counting unsynced foreign records
(`write_parallel_does_not_rely_on_another_stores_unsynced_records`);
`compact` without its pause of local writers, inside a sweep
(`compact_inside_a_sweep_waits_for_local_write_spans`); a cycle keeping the
gate after a failed mark (`a_failed_mark_releases_the_gate`); a sweep
ignoring its cancellation while it waits for a span of its own store
(`begin_sweep_honours_cancellation_behind_a_local_span`; both cancellation
tests now run the sweep on a bounded thread, where the first used to hang on
its mutant instead of failing). Most of these gaps are Go's as well. Known
and left: "the first write span always refreshes" cannot be told from "the
generation moved" by a test, because any sweep leaves a generation of at
least 1 in the file and a store starts at 0; the re-check of the final name
in `create_active` has no test (the one in `adopt` has).

Also from the review: segment ids wrap, as Go's `uint64` does — a name with
the highest id, a stale temporary included, used to panic a debug build's
first write (`highest_segment_id_does_not_stop_the_first_write`); sealed
paths are derived with `Path::with_extension`, not through a lossy string,
so a store under a directory whose name is not UTF-8 works; a panic in the
refresh under the gate's busy step releases the step and the file lock
(`a_panic_in_the_refresh_does_not_hold_the_gate`; Go would crash); the two
tests that start a child process kill it after a minute, so that a child
that hangs fails its test.
