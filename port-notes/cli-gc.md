# cli-gc — port notes

Port of the pin..HEAD `cmd/amber-store` delta (Go `a2ff135`: `gc.go` new;
`main.go`, `store.go`, `ref.go`, `ingest.go` collector wiring;
`e2e_test.go` additions) into `examples/amber-store.rs` and the new
`tests/cli_e2e.rs`.

## What changed in the example

- **Global `--segment-size`** (default `packstore::DEFAULT_SEGMENT_SIZE`,
  usage verbatim from Go) threaded into `open_store`'s
  `packstore::Options` next to `sync(true)`.
- **`gc` subcommand** (`gc status` / `gc run` / `gc why KEY`) with the Go
  format strings byte-for-byte — the bench greps `gc status` lines by the
  `"live "` / `"last cycle"` prefixes and `gc run` output for `"reaped"`.
  `gc why` takes its KEY as a `Vec<String>` positional so the Go arg-count
  error (`"gc why requires exactly one KEY argument, got %d"`) is
  reproduced through the normal error path instead of a clap usage error.
- **`ref set` / `ref rm` / `ingest --ref`** route through the collector:
  `put_ref` (read old -> `prepare_ref` -> `refs.put` -> `commit`, `abort`
  on put failure, `release_ref(old)`) and `rm_ref` (resolve root, delete,
  `release_ref`), with Go's exact wrapping texts
  (`"existing reference {name:?}: ..."`, `"reference {name:?}: ..."`).
- **Store plumbing**: `Stores` now holds `Arc<packstore::Store>` /
  `Arc<refstore::Store>` because `gc::Collector::open` takes Arcs (see
  `port-notes/gc.md`). `open_collector` opens `<store>/closures` from the
  same `--store`/$AMBER_STORE resolution (`store_dir`). Teardown order is
  collector close -> **drop** (releases its store Arcs; the refstore only
  really closes when the last Arc drops) -> `close_store`. `join_errs`
  mirrors `errors.Join(err, coll.Close(), closeStore(...))` — messages
  joined by newlines; `gc status/run/why` instead *discard* close errors,
  exactly like Go's bare `defer`s.
- **Helpers**: `human_bytes` ports `humanBytes` from Go `progress.go`
  (including the 1023.95 unit-promotion); `rfc3339_local` renders
  `SystemTime` the way Go formats a local-zone `time.Time` with
  `time.RFC3339` (numeric offset via `tm_gmtoff`, `"Z"` for offset 0) for
  the pack `SEALED` column and `last cycle:` start.

## Go-compatible durations (no new crates)

- `parse_go_duration` is a full port of Go `time.ParseDuration`
  (`leadingInt`/`leadingFraction` included): ns/us/µs(U+00B5)/μs(U+03BC)/
  ms/s/m/h, decimals, concatenations (`1m30s`), leading sign, bare numbers
  rejected except `"0"`, the same `1<<63` overflow walls, and Go's error
  texts (`%q` rendered as `{:?}`). Used as the clap value parser for
  `gc run --grace` (default `"1h"` parsed through it;
  `allow_hyphen_values` keeps Go's negative-duration acceptance).
- `format_go_duration` ports `Duration.String` (buffer-from-the-end
  `fmtFrac`/`fmtInt`, `0s`, `999ns`, `1.5µs`, `12ms`, `1.234s`, `1m3.5s`,
  `1h0m0s`, negative sign) and `round_ms` ports
  `Duration.Round(time.Millisecond)` (half away from zero, saturating).
  Spot-checked against Go vectors, including `"10.5s4m"` = 250.5 s and the
  `1<<63` parse boundaries.

## Deviations from Go (and why)

1. **Flag-parse failures exit 2** (clap) where urfave/cli returns them
   through `app.Run` (exit 1): pre-existing example precedent; the ported
   tests only assert success/failure. The `--grace` parse errors carry
   Go's message text inside clap's `error: invalid value ...` wrapper.
2. **`--segment-size` is `u64`** (Go: Int64); clap rejects negatives at
   parse time instead of handing packstore a negative size.
3. **`--grace` negatives clamp to `Duration::ZERO`** when building
   `gc::Options` — same outcome as Go, whose `withDefaults` maps
   `Grace <= 0` to the 1 h default.
4. **No `max(x, 0)` clamps** on `GarbageBytes`/`FreedBytes` before
   humanizing: the Rust gc counters are unsigned (`port-notes/gc.md`
   deviation 1), so the clamp has no counterpart.
5. `gc status`/`run`/`why` print to the process stdout (the example has no
   `app.Writer` indirection); errors go to stderr via the existing
   `"amber-store: {e}"` main, exit 1 — `gc why` on an unreferenced key
   prints `"unreferenced"` and exits 0, as in Go.
6. `run_gc_run` prints via `println!` after mapping the stats — Go prints
   with `fmt.Fprintf` before its deferred closes run; ordering is
   equivalent (close errors are discarded in both).

## tests/cli_e2e.rs

Ports `TestE2E_RefSetChecksCompleteness`, `TestE2E_RefLifecycle`,
`TestE2E_GC`, **and `TestE2E_MissingStoreFlag`** (pre-pin, but it had no
Rust counterpart — the tests/ tree had no CLI coverage at all). Go drives
`newApp()` in-process; the Rust tests spawn the example binary: located
from `current_exe()` (`target/<profile>/deps/x` ->
`../examples/amber-store`), built via `env!("CARGO")` (`--release` added
when the profile dir says so) if a bare harness invocation finds it
missing — `cargo test` itself always builds examples first. `run_app`
forces `AMBER_STORE=""` (Go's `t.Setenv` in the missing-store test;
everything else passes `--store`). The Go shape is kept: `--segment-size
4096` on every store call (`run_seg`), the two/one 50 ms sleeps so seals
cross `--grace 1ms`, `--garbage 0` forced runs. 5/5 repeat runs green,
~1 s each.

## `--expect` on `ref set` and `ref rm` (Go PR #13)

Port of the `cmd/amber-store/ref.go` delta at Go `91da3cf` and of
`TestE2E_RefExpect` (`ref_expect` in `tests/cli_e2e.rs`).

- `Expectation` is an enum (`Unconditional` / `Absent` / `At(Key)`) where Go
  has a struct of two booleans and a key; `parse_expect` and `explain` carry
  Go's rules and texts: `--expect` given but empty is an error (a script's
  unset variable must not turn the write into an unconditional one),
  `--expect none` is refused by `ref rm`, a malformed key is wrapped as
  `--expect: …`; a conflict reads `reference "r" already exists: …` or
  `… does not point at KEY: …`, an absent reference `… does not exist,
  expected it at KEY: …`, each followed by the store's own message.
- **Flags go before the positionals**, as urfave/cli parses them. clap
  accepts options anywhere by default, so `ref set` and `ref rm` take their
  positionals as one `Vec<String>` (the device `gc why` uses to get Go's
  argument-count error out of the normal error path, exit 1) marked
  `trailing_var_arg`: whatever follows the first positional is a positional,
  so a misplaced flag fails the count (`ref set requires NAME KEY arguments,
  got 4`). The first version also set `allow_hyphen_values`, which with clap
  4.6 has the same effect on its own; the review found what else it does:
  `ref set -lead KEY` created a reference called `-lead`, where Go refuses
  the unknown flag. Without it clap refuses too (`unexpected argument`), and
  as in Go a name that begins with a dash goes after `--`; `ref_expect` pins
  both. The Rust test adds the case Go's cannot tell apart: a misplaced `--expect`
  carrying the *current* key, which a parser that honoured it would let
  through.
- `put_ref`: `Unconditional` → `put`, `Absent` → `create`, `At` →
  `compare_and_swap`; on failure `abort` and `explain`. After a conditional
  put what is released is the expected key (or nothing, for `Absent`),
  whatever the earlier read saw. `rm_ref` likewise deletes with
  `compare_and_delete` and releases the expected key. `ingest --ref` and
  `commit create --ref` stay unconditional, as in Go.

## Write spans (Go PR #14)

- `RefGate` is Go's `refGate`: `prepare_ref` and `release_ref`, implemented
  for `gc::Collector` (which opens a span for the one put) and for `gc::Span`
  (a span already open around the writes the reference names). `put_ref`
  takes `&dyn RefGate`; `rm_ref` keeps the collector, as in Go.
- `open_span` is Go's `openSpan`: the collector's span (`begin_span`,
  reference lock before the gate) when a reference is to be put, the store's
  own (`packstore::Store::begin_write`) otherwise, and then no collector is
  opened at all. One difference is forced by borrowing: Go's `cliSpan` opens
  and closes the collector itself, while a `gc::Span<'c>` borrows its
  collector. So the caller opens the collector first (`collector_for`), runs
  the part inside the span as a function of its own (`ingest_in_span`,
  `commit_in_span`) and closes the collector afterwards (`close_collector`).
  Order on the way out: span, collector, stores.
- `ingest` brackets `ingest::dir` and the reference put; `commit create`
  brackets the `has` checks on the tree and the parents, the put and the
  reference put. `ref set` needs no span of its own: `Collector::prepare_ref`
  opens one for the put.
- A failed collector close is reported when the command otherwise succeeded
  and dropped behind the command's own error; Go's `defer span.end()` drops
  it either way.

## For later waves

- The bench (`cmd/amber-bench`) duplicates `putRef`; the reference Rust
  shape is `put_ref` in `examples/amber-store.rs` (and `put_test_ref` in
  `src/gc/tests.rs`). It also needs `human_bytes` and the two duration
  helpers — they live in the example only; copy or lift them into a shared
  helper if the bench wants them (examples can't be imported).
- `gc status` output includes the pack `SEALED` column as **local-zone**
  RFC3339 (offset suffix), so its width exceeds the `%-20s` pad except in
  UTC — identical to Go; don't "fix" the column alignment.
- No gaps found in the wave-2 gc surface: `Collector::open/close`,
  `prepare_ref`'s commit/abort handle, `run(garbage)`, `status()`, `why`,
  `release_ref` covered everything the CLI needs with the documented
  signatures.
