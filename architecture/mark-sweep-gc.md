# Mark-and-Sweep Garbage Collection

A port of Mic92's bitmap GC from
[draganm/amber-store#9](https://github.com/draganm/amber-store/pull/9),
replacing the simple-gc collector (per-root closure files, an in-RAM
union of tails, refcount bookkeeping on every reference write — see git
history for `architecture/simple-gc.md`). Live is what a reference
reaches; the rest is garbage. Nothing about liveness is persisted or
incrementally maintained: every cycle recomputes it from the references,
marks it into bitmaps, and sweeps.

Goals, in order: an accepted reference never dangles — a reference write
is optimistic and may have to be retried instead; ingests never wait for
the mark (only for the sweep); no auxiliary state between cycles — no
closure files, no union, no refcounts, nothing to rebuild or to corrupt;
no format change; consistent after a crash at any point.

## The mark set

`packstore.MarkSet` is a liveness mark over a snapshot of the store:

- **One bit per sealed record.** A sealed pack's footer index already
  assigns every record a dense position; the mark set holds one `[]uint64`
  bitmap per pack, indexed by footer position. 200 packs × 256 MiB of
  ~128 KiB records cost a few KiB of bitmap each — the whole mark is a
  few MiB of RAM regardless of how many references share the objects.
- **A map for the active segment**, whose index is in RAM anyway.
- `Mark(k)` locates k exactly as the read path does (newest pack first,
  binary-fuse filter, then the footer's fanout index) and sets its bit,
  reporting whether it was newly marked and whether it is present at all.

## The cycle

```
refLock.Lock    barrier on; snapshot the reference roots   (writers stall: µs)
                mark: walk every root's tree into the      (concurrent with
                mark set, pruning at marked keys;           ingests and reads)
                blobs/xattrs are marked without being read
refLock.Lock    sweep: packstore.Compact                   (writers stall)
```

- **Mark.** For each root in the snapshot, a DFS over `fstree.ChildKeys`.
  Already-marked keys prune the walk, so shared subtrees cost once. A
  missing object aborts the cycle loudly.
- **Sweep** (`packstore.Compact`): seal the active segment; select as
  victims the sealed packs whose dead bytes reach the garbage line
  (`--garbage`, policy 0.5, or 0.1 under min-free pressure) *and* whose
  seal time is older than the grace period (`--grace`); copy each
  victim's live records — skipping any that already exist outside the
  victims — into the active segment, fully re-verifying (CRC, key match,
  payload rehash) in parallel while a single loop appends; fsync; only
  then unlink the victims and fsync the directory. A crash mid-sweep
  loses no live record: victims outlive the durable copies.

## Writers vs. the cycle: the barrier

The mark runs against a snapshot, so anything that becomes live during
the mark must be protected (specs/gc.qnt, policy "barrier"):

- **Object writes.** `BeginBarrier` starts grey capture: every key a
  `Put`/`WriteBatch`/`WriteParallel` observes — written *or dedup-hit* —
  joins a grey set. `Compact` consumes the set and treats grey as live.
  The dedup-hit case matters: an ingest that skips writing an object
  because a condemned pack already holds it would otherwise lose it.
- **Reference writes.** `PrepareRef` holds the collector's reference lock
  shared from its completeness walk to its commit; the cycle holds it
  exclusively around the roots snapshot and again around the sweep. A PUT
  in flight at snapshot time therefore commits (and is marked) or aborts
  first; a PUT landing during the mark proceeds — its walked closure is
  handed to the grey set (`ObserveKeys`), so even a reference naming a
  pre-existing, unmarked tree survives the sweep it races.
- **Reference deletes** are just refstore deletes. No bookkeeping: the
  next cycle no longer marks from the dropped root.

What remains optimistic: an upload whose dedup hits land *before* the
barrier began and whose reference PUT lands *after* the sweep can find
its objects reaped; the PUT's completeness walk fails cleanly (the 404)
and the client re-ingests. simple-gc shielded this window with upload
leases; this collector accepts the retry instead — grace keeps the
window away from everything younger than `--grace`.

## Cost model, vs. simple-gc

| | simple-gc | mark-and-sweep |
| --- | --- | --- |
| reference PUT | closure walk + closure file write + O(union) merge | closure walk only |
| reference DELETE | O(union) merge per release | nothing |
| cycle | score every pack's index against the union; copy | full mark walk (reads every live tree's metadata), then select + copy |
| state between cycles | closures on disk + union in RAM | none |
| `gc status` | union lookup per index entry | a full mark walk first |

The trade: churn-time work (every PUT and DELETE paid an O(union) pass)
moves into the cycle, which now pays a full mark walk — reads of every
live tree object (blobs are marked from the index alone, never read).
The copy loop itself is the same job in both designs; here it re-verifies
payloads while copying and batches its fsyncs at the end.

## Across processes

Everything above holds inside one process: the grey set and the reference
lock are memory. Stores in other processes are held to the simpler policy the
model also proves safe, *quiesce* (`specs/gc.qnt`). From before the roots
snapshot until after the sweep the cycle holds the packstore's gate
(`gc.lock`, [packstore.md](packstore.md#the-gate-writers-against-a-sweep))
exclusively, and every write span and reference PUT elsewhere holds it shared.
So while a cycle runs, writers and PUTs in other processes wait, readers never
do, and writers in the collector's own process run through the mark behind
the barrier exactly as described above.

- The gate is taken under the cycle's first reference lock and dropped under
  its last, with no local span in flight: a file lock cannot be turned from
  shared into exclusive atomically, so it is never converted. The wait for
  other processes' spans therefore happens with local writers stalled: the
  "mark waits for ingests to drain" moment, extended to other processes.
- Having taken the gate, the collector lists the store directory once more;
  its view is then complete and stable. The mark set covers every active
  segment, its own and others'.
- The sweep seals the active segments no writer holds, besides the
  collector's own, so that a small store, whose segments never fill, still
  gets collected. A segment held by a live writer is left alone.
- Whoever takes the gate exclusively moves a generation in `gc.lock` on,
  counting from the file, before it deletes anything. A store that finds it
  moved, and any store on its first write span, lists the directory before its
  next duplicate check, which could otherwise hit a record in a segment that
  was reaped under it.
- Objects written by another process and not yet referenced when a cycle
  starts are protected by the grace period alone. A writer that wants more
  brackets its writes and the reference PUT in one span,
  `Collector.BeginSpan`, as `amber-store ingest --ref` and `commit create
  --ref` do. The span takes the reference lock and then the gate, the order a
  cycle takes them in, and prepares references without taking either again
  (`Span.PrepareRef`). A `packstore.BeginWrite` span held across
  `Collector.PrepareRef` takes them the other way round and can deadlock
  against a cycle of the same process.
- Spans nest: a write inside an open span never waits for a sweep that is
  waiting for that span.
- `Status` looks at the store directory again before its advisory mark. The
  references it marks from are everybody's; the view of the objects is its
  own store's, as of that store's last look.
- A store that just swept leaves the gate alone for 100 ms before sweeping
  again; back-to-back sweeps would otherwise starve a writer that polls for
  the lock from another process.

## Layout

```
<store>/packstore/   segments, the sidecar indexes of active ones, gc.lock (packstore.md)
<store>/closures/    kept, empty: swept at open (simple-gc migration)
<store>/refs/        refs.sqlite (references.md)
```

A store previously run under simple-gc opens cleanly: closure files are
derived data and are deleted at collector open.
