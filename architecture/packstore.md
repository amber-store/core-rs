# The packstore on disk, and sharing it between processes

The packstore keeps CAS objects in log-structured, append-only **segment**
files. Records are the ones [amberpack.md](amberpack.md) defines; this document
covers the directory, the index beside an active segment, and the rules that
let any number of processes read and write one store at once. It is written so
that a second implementation can share a store with this one; what it must
keep to is gathered under [Rules for implementations](#rules-for-implementations),
which also names the one layout this document does not give. All integers are
big-endian.

## The directory

```
<packstore>/
  0000000000000007.seg                  a sealed segment: immutable, self-indexed by its footer
  0000000000000009.seg.active           an active segment: the header, then records, appended
  0000000000000009.seg.active.idx       its sidecar index
  000000000000000a.seg.active.tmp       an active segment still being created
  gc.lock                               the gate between writers and a sweep; holds the view generation
```

A segment's name is its id, sixteen hex digits. Sealed and active segments
share one id space. Several active segments may exist: one per writer that was
at work at the same time, and whatever they left behind. Anything else in the
directory is ignored.

A sealed segment is `AMBERSG\x01`, the records, and a footer (seal marker,
fanout index, binary fuse filter, 64-byte trailer ending in `AMBERSGF`); an
active one is the same without the footer. Sealing appends the footer, fsyncs,
renames `<id>.seg.active` to `<id>.seg`, fsyncs the directory and removes the
sidecar. Closing a store does not seal.

## The sidecar index

An active segment's index lives in its owner's memory. The sidecar mirrors it
on disk, so that opening the segment — by its next owner, or by any reader —
does not mean parsing the data file. It is `AMBERIX\x01` followed by 56-byte
records:

```
offset  size  field
0       1     kind   0x01 entry, 0x02 synced
1       32    key    entry: the record's key            synced: zero
33      8     off    entry: offset of the record header synced: data length known durable
41      1     flags  entry: the record's flags byte     synced: zero
42      4     ulen   entry: uncompressed payload length synced: zero
46      4     slen   entry: stored payload length       synced: zero
50      2     zero   reserved
52      4     crc    CRC-32C (Castagnoli) of bytes [0:52]
```

**Writing**, by the segment's owner only:

1. A record is written to the data file first; its **entry** is appended only
   after that write returned.
2. After an fsync of the data file returned, a **synced** record carrying the
   length that fsync covered is appended. A synced `Put` therefore leaves an
   entry and a synced record, a batch one synced record per fsync, and `Close`
   (which always fsyncs) one more.
3. The sidecar itself is never fsynced. It is a cache: losing its tail costs a
   longer scan, never data. A failed sidecar write does not fail the store's
   write, but it stops the sidecar for good, so that no record ever follows a
   hole.

**Reading**, by anybody. Read the sidecar first and the data file's length
after it, so that the data covers whatever the sidecar speaks of.

1. Take records while the CRC holds; stop at the first bad or partial one. Let
   *S* be the largest data length in a synced record (the header's length if
   there is none). A synced record that claims more than the file holds
   discredits the whole sidecar.
2. Entries describe consecutive records. One that does not start where the
   previous one ended, or runs past the file, ends what the sidecar can vouch
   for.
3. An entry whose record ends at or before *S* is **trusted** unread: that
   data was durable before the entry was written.
4. Later entries are **verified**, in order, by parsing their record (framing,
   CRC, and the key and lengths the entry names). The first failure ends the
   valid data.
5. What follows the last good entry is **scanned** record by record until the
   first invalid byte or a seal marker. This finds records written, perhaps
   acknowledged, just before a crash kept their entries from the sidecar.

A missing, unreadable or discredited sidecar means a scan of the whole data
file. A file that ends in the trailer magic may be a seal that crashed before
its rename; the whole-file scan decides. After a clean close, or while the
owner syncs its writes, opening reads the sidecar and nothing of the data.
Measured on the development Mac (warm cache, 4 KiB objects), opening a store
whose active segment holds 64 MiB, 256 MiB and 1 GiB took 13, 52 and 197 ms
when every open scanned the data, and 1.6, 6.2 and 29 ms with the sidecar;
`CLAUDE.md` has the whole table.

A reader never modifies either file. Only the owner truncates a torn tail,
finishes a crashed seal or rewrites the sidecar, and it does so when it takes
the segment: it keeps the part of the sidecar that agrees with the data, drops
the rest, and appends entries for the records the scan found. An index whose
data file is gone (a crash between a seal's rename and the sidecar's removal)
is deleted by whoever opens the store next.

## Owning an active segment

A writer owns an active segment by holding an exclusive `flock(2)` on its
**data file**, from the moment it takes the segment until it closes the file
or seals the segment. Nothing is locked when a store is opened, so a process
that only reads, a pager left open for instance, never keeps a segment from
the next writer.

At its first write a store **adopts before it creates**. It lists the active
segments, fullest first, and tries a non-blocking lock on each; having locked
one, it checks that the file is still the one at that path (it may have been
sealed or reaped since the listing), recovers it with the owner's rights and
appends to it. Only when every active segment is held by a live writer does it
create one. Serial writers therefore keep filling one segment to the segment
size; the number of active segments is bounded by the peak number of
simultaneous writers and drifts back to one as they fill.

**Creating** a segment with an id above every id in the directory:

1. Create `<id>.seg.active.tmp` with `O_EXCL`. If it exists, another creator
   is at this id: take the next.
2. Lock it, without waiting, and check that it is still the file at that path.
   If either fails, a store that was clearing away crash leftovers took it for
   one: take the next id.
3. Check that neither `<id>.seg.active` nor `<id>.seg` exists. The temporary
   name is free again as soon as its creator has renamed it, so holding it
   does not prove the id is new.
4. Write the header, fsync, rename to `<id>.seg.active`, fsync the directory,
   create the sidecar.

Nobody ever sees, or adopts, a segment that is not ready and owned. A later
creator removes temporary files it can lock.

## What a store sees

At open a store maps every sealed segment and indexes every active one by the
reading rules, locking nothing. A file under an active name that carries a
whole footer is mapped as the sealed segment it is; its next owner renames it.

The view goes stale as others write. **A lookup that finds nothing lists the
directory again** and then looks once more: sealed segments that appeared are
mapped, those that vanished are let go (a mapping stays valid after its file
was unlinked, so nothing breaks in between), other writers' segments are read
further from where the last look stopped. A segment is identified by its id
**and** its file: a repair replaces a sealed segment under its name, and an id
can come back once the highest segment was reaped.

Listing costs a stat per segment, and a lookup that finds nothing is common
(any "do you have this?"), so the listing is skipped when nothing can have
changed: the directory's modification time is the one read just before the
last listing, and no active segment this store reads has changed size
(appending to a segment does not touch the directory, hence the size check).
A modification time proves this only if it was at least two seconds old when
the directory was listed. On a filesystem with coarse timestamps a segment
created in the same tick as the listing would otherwise go unnoticed for good.
So for two seconds after any change to the directory every lookup that finds
nothing lists it; the few microseconds `CLAUDE.md` reports for such a lookup
are the steady state.

A refresh takes effect whole or not at all. One that fails half way — a new
segment it cannot read, say — leaves the view as it was, including how far it
had read into other writers' segments: a position that moved on without the
entries it passed would lose them for good. And if something the refresh
listed is gone when it comes to open it, the listing is out of date: what that
segment held may be in one created since, a compaction's copy or a seal's new
name. The refresh then lists again, up to three times, rather than settle for
a view with a hole in it.

The write path's duplicate check does not look again. A duplicate it fails to
see costs a redundant record, which compaction folds; a directory listing for
every new object would cost every ingest dearly. A bulk "which of these are
missing" call looks once, up front.

**A write relies only on what is durable.** A store that syncs its writes
counts a record in another writer's active segment as a duplicate only as far
as that writer has synced it, which the `synced` entries of the sidecar tell:
its own fsync covers its own segment alone, and acknowledging a write against
bytes somebody else has yet to sync would promise what nobody has delivered.
Otherwise it writes a copy of its own, which compaction folds later.
Compaction applies the same rule to survivors: a copy a live writer has yet to
sync does not excuse deleting a segment that holds a synced one. Reads report
what is visible: `Has`, `Get` and `Missing` wait for nobody's fsync.

A read from another writer's active segment checks that the record at the
indexed offset carries the key and length the index named; if not, the view is
rebuilt, and failing again the read reports corruption rather than returning
another object's bytes.

## The gate: writers against a sweep

A GC cycle must not lose an object a writer just relied on, whether written,
or skipped as a duplicate of a record the cycle is about to reap. Inside one
process the write barrier and the collector's reference lock see to that
([mark-sweep-gc.md](mark-sweep-gc.md)). Across processes `gc.lock` applies the
simpler safe policy, *quiesce*, with one `flock(2)`:

- **Shared**, while any write span of a store is in flight: every write, and
  any larger span a caller brackets, such as a completeness walk followed by a
  reference put, or an ingest followed by the reference put that names it.
- **Exclusive**, for a whole GC cycle, from before its snapshot of the
  reference roots until after its sweep, and for anything else that deletes
  segments.

So while one process sweeps, writers and reference puts in the others wait;
readers never do. Writers in the sweeping process itself do not take the file
lock while their process holds it exclusively: the barrier and the reference
lock deal with them.

`flock` cannot turn a shared lock into an exclusive one atomically (the lock is
dropped in between, and another process may take it), so an implementation
**never converts**: it takes and drops the exclusive lock only when none of
its own write spans is in flight, and makes new ones wait meanwhile. Waiting
for the lock is polling with a back-off of at most 50 ms; a process that just
swept leaves the lock alone for 100 ms before sweeping again, or a writer
polling from elsewhere could be starved by back-to-back sweeps.

**Spans nest.** A span that finds another span of its store in flight joins it
without waiting, even while a sweep of that store is waiting for the spans to
end: the inner span may be the outer one's own work, a write inside a
bracketed span, and the two would otherwise wait for each other. A sweep
therefore starts at a moment with no span in flight. A process that writes
without pause from many goroutines and also sweeps quiesces its writers
itself, as the collector does with its reference lock.

**The view generation.** The first 8 bytes of `gc.lock` are a counter. Whoever
takes the lock exclusively reads the counter **under the lock**, writes one
more, and only then deletes anything (it is not fsynced: it only has to
outlive the processes that are running). Counting from the file matters: a
store that added one to the value it remembered would, after another store's
sweep, write the current value again, and nobody would notice. Writing before
deleting matters: a sweeper that dies half way has still told everyone. A
store that cannot write the counter does not sweep.

Each time a store takes the shared lock it reads the counter, and if it moved
it lists the directory again **before anything else**. It also does so on its
first write span, whatever the counter says: it may have opened in the middle
of a sweep and listed a directory that was about to change. Without this, a
store that still maps a reaped segment would skip an object as a duplicate of
a record that is gone.

**What a cycle sees.** Having taken the exclusive lock, the collector lists
the directory; from then on its view is complete and stable. Its mark set
covers every active segment it can see, not only its own. The sweep seals the
collector's own active segment and every one it can adopt, so that a small
store, whose segments never fill, still gets collected; a segment a live
writer holds is left alone, and an active segment is never a victim.

Objects written by another process and not yet named by a reference when a
cycle starts are protected by the grace period alone. (git has a second
defence that this store lacks: a write that finds its object already present
freshens the file's time. Here a duplicate of a dead record in an old segment
gets no new grace. What catches a reaped object is the completeness walk of
the reference put that follows, which fails.) A writer that wants more
brackets its writes and the reference put that names them in one write span.
In a process that runs a collector that is `gc.Collector.BeginSpan`, which
takes the collector's reference lock before the gate, the order a cycle takes
them in ([mark-sweep-gc.md](mark-sweep-gc.md#across-processes)).

`Wipe` refuses, deleting nothing, while another store owns an active segment:
that writer would go on appending to a file that is gone.

## Rules for implementations

What another implementation sharing a store has to keep to, beyond the layouts
above:

- **Records start at byte 8** of a segment, after the header magic, and are
  contiguous.
- **A record before its sidecar entry, a `synced` entry only after fsync
  returned**, and the sidecar itself is never synced.
- **Own a segment before writing to it**: the non-blocking exclusive `flock`
  on the data file. Create one under its temporary name, exclusively, lock
  it, **then check that the locked file is still the one at that name** (same
  device and inode), and that no segment has the id; whoever clears away
  stale temporaries makes the same check after taking their lock.
- **Never convert the gate's lock**, count the generation from the file and
  write it before deleting anything, and refresh on a moved generation and on
  the first span, as [above](#the-gate-writers-against-a-sweep).
- **Survivors are durable before victims are unlinked**: a compaction syncs
  the segment it copied into, then removes the old ones, then syncs the
  directory.
- **Rely only on durable copies** when skipping a write or a survivor's copy.

The **sealed footer** (index section, filter, trailer) is older than this
document and is not laid out here yet; `packstore/footer.go` is the reference.

## Known limits

- A process that is stopped while it holds `gc.lock` stalls every writer.
  There is no timeout and no diagnostic.
- A sweeper waits for as long as other processes' write spans overlap; nothing
  bounds that wait.
- A sidecar is not bound to the identity of its data file. A release from
  before sidecars that truncated a segment and grew it again under the same
  id would leave a stale sidecar that could still look plausible. Reads from
  another writer's segment check the record's key and would notice; reads
  from a store's own segment do not.

## Compatibility

Sealed segments are unchanged, and a store written by an earlier release opens
as it is: its single active segment has no sidecar, is scanned once, and gets
one from the writer that adopts it.

Releases from before stores could share a directory take an exclusive,
non-blocking `flock` on the directory itself and assume they own the one
active segment. Every store therefore holds that lock **shared** for its whole
life: the old exclusive attempt fails while a new store is open, and a new
store fails to open while an old binary holds the directory. An old binary
that finds two active segments refuses the directory.

All of this rests on `flock(2)` and on processes sharing a page cache: one
host, a local filesystem.
