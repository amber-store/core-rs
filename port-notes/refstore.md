# refstore — port notes

Port of Go `refstore/` at `91da3cf` (Go PR #13, "reference store on SQLite,
optimistic updates"): `refstore.go`, `sqlite.go`, `schema.go`, `cas.go`,
`queries.sql`, `migrations/`, and `migrate.go` re-thought for this crate's
own legacy. Public API: `Store` (`open` / `put` / `put_batch` / `get` /
`delete` / `all` / `wipe` / `compare_and_swap` / `create` /
`compare_and_delete`), `Record {name, data}`, `Error` with `is_not_found()`
and `is_conflict()`, `SchemaError`. `Close()` stays RAII.

| File | Go | Holds |
| --- | --- | --- |
| `src/refstore/mod.rs` | `refstore.go` | `Store`, `Error`, the connection model, the unconditional operations |
| `src/refstore/cas.rs` | `cas.go` | the optimistic writes |
| `src/refstore/sqlite.rs` | `sqlite.go` | file name and format constants, per-connection pragmas, the busy retry, the WAL check |
| `src/refstore/schema.rs` | `schema.go` | the embedded migrations and their runner, `SchemaError` |
| `src/refstore/queries.rs` | `queries.sql` + `internal/refsdb` | the statements |
| `src/refstore/migrations/` | `migrations/` | byte-identical copies |
| `src/refstore/migrate.rs` | `migrate.go` | the redb import, the poison, the Pebble refusal |

## One file format, shared with Go

Until this port the two implementations differed by design (Go: Pebble,
Rust: redb) and could not open each other's `refs/`. Now both read and write
`<dir>/refs.sqlite`, and `architecture/references.md` — copied byte for byte
from the Go repository, never edited here — is the contract. Its "Rules for
implementations" are followed to the letter:

- `PRAGMA application_id = 0x616D6272`, `PRAGMA user_version` = number of
  migrations applied, table `refs (name BLOB NOT NULL PRIMARY KEY, record
  BLOB NOT NULL) WITHOUT ROWID`. WAL is mandatory and verified at open
  (`Error::JournalMode` otherwise).
- **Migrations.** `src/refstore/migrations/*.sql` are byte-identical copies
  of Go's files, under the same naming rule (`NNNN_description.sql`,
  contiguous from 0001), applied inside ONE `BEGIN IMMEDIATE` transaction
  that ends by setting `user_version`. Fresh = application id 0, version 0,
  no schema objects. Refused: a foreign application id (0 on a non-empty
  file included), a negative version, a newer version. An up-to-date
  database is recognised from plain reads, without taking the write lock;
  after taking the lock the state is read again and the loser of a race
  returns without writing. Go embeds the directory (`embed.FS`);
  `include_str!` takes literal paths, so `schema::EMBEDDED` is a written-out
  list, and the unit test `embedded_migrations_match_the_directory` compares
  it — names and bodies — with the directory, so a file added without being
  listed fails the build's tests.
- **Statements.** There is no sqlc for Rust. `queries.rs` keeps each
  statement as a constant holding exactly the text sqlc renders into Go's
  `internal/refsdb/queries.sql.go` — the `-- name:` line, the `?1`-style
  numbering of `sqlc.arg(...)` and the trailing newline included — so both
  implementations send SQLite the same statements. When Go's `queries.sql`
  changes, copy the rendered constants again.
- **BLOBs always.** rusqlite binds `&[u8]` as a BLOB and an empty slice as a
  zero-length BLOB (`sqlite3_bind_zeroblob`), never NULL; names are bound as
  `name.as_bytes()`, never as TEXT. Pinned by
  `empty_name_and_empty_record_round_trip` (`typeof()` of what put,
  put_batch and create stored).

## Driver

`rusqlite` 0.40 with the `bundled` feature: SQLite (3.53.2 today) is compiled
into the crate, as `modernc.org/sqlite` compiles it into Go, so there is no
system library to vary. The bundled build sets `HAVE_USLEEP`, which the busy
timeout needs for sub-second sleeps. Of rusqlite's default features only
`cache` is on, for `prepare_cached`; the other, `ffi-sqlite-wasm-rs`, would
add ten wasm-only packages to `Cargo.lock` for a target this crate does not
build for. The path is taken literally, so `?`, `#`, `%` and
spaces in it need no escaping (Go escapes them into a `file:` URI; test
`open_path_with_special_characters`). Not passing `SQLITE_OPEN_URI` is not
enough for that: the bundled library is compiled with `SQLITE_USE_URI`, so it
parses any name that begins with `file:` as a URI regardless. Only a relative
path can begin like that, and `sqlite::literal` puts `./` in front of it
(`a_path_that_looks_like_a_uri_is_taken_literally`).

## Connection model

`rusqlite::Connection` is `Send` but not `Sync`, and the crate shares one
`Arc<refstore::Store>` between threads (gc, the CLI, the bench), so:

- **One writer connection behind a `Mutex`.** This is Go's `writeMu`:
  in-process writers queue on the mutex instead of polling SQLite's file
  lock. It is the connection `open` did its work on.
- **A pool of reader connections** (`Mutex<Vec<Connection>>`): a read pops an
  idle connection or opens a new one, runs without holding the pool mutex,
  and pushes the connection back; at most 8 stay idle, and a connection whose
  read failed is closed instead of reused. `get` and `all` therefore never
  wait behind a writer — not even one that is itself waiting for another
  process (`reads_do_not_wait_for_a_queued_writer`).
- Statements are `prepare_cached` per connection.
- Every connection sets the busy timeout FIRST (30 s), then the journal mode,
  then `synchronous=FULL` + `fullfsync=1` + `checkpoint_fullfsync=1` when
  `sync` is true, `synchronous=NORMAL` otherwise — the readers too
  (`sync_flag_selects_durability_pragmas` checks both kinds).
- `put`, `delete`, `create` and `wipe` are single statements in autocommit
  mode, exactly as in Go; `put_batch`, the compare forms, the schema runner
  and the import are `BEGIN IMMEDIATE` transactions.

**The WAL switch.** Switching a database into WAL mode takes an exclusive
lock for which SQLite does NOT run the busy handler, so concurrent first
opens fail with SQLITE_BUSY. `sqlite::connect` retries that — 1 ms doubling
to 50 ms, bounded by the busy timeout — as Go's `connect` does. Pinned by
`concurrent_first_opens` (8 threads behind a barrier, 10 rounds) and
`concurrent_first_opens_across_processes`.

**The write lock never leaks.** Go defers `tx.Rollback()`; here a
`rusqlite::Transaction` rolls back when dropped, and in every write path it
is declared after the writer's mutex guard, so it is dropped first: each
early return — not found, conflict, an undecodable record, a failing
migration, a refused version — and a panic unwinding through the change
leave no transaction behind. The panic poisons the mutex; `writer()` ignores
the poison, because by then the connection is clean. Tests:
`write_transactions_survive_a_panic`,
`refused_compare_forms_release_the_write_lock`,
`failed_migration_changes_nothing`, `negative_schema_version_is_refused`.

## Optimistic forms

`compare_and_swap(name, old_key, record)`, `create(name, record)`,
`compare_and_delete(name, old_key)`, as `cas.go`: inside one IMMEDIATE
transaction the current record is read and decoded with `reference::decode`,
its key compared with `old_key`, and the change is guarded by the bytes just
read (`… AND record = ?`); a change that touches no row is a conflict. A
reference that does not exist is `Error::NotFound`, one that points
elsewhere — or exists, for `create` — is `Error::Conflict`, and a current
record that does not decode is `Error::CurrentRecord`, never a match (Go:
`refstore: current record of %q: %w`; the name is quoted by the crate's
`tarexport::GoQuote`, exact for ASCII). The store retries nothing.

## The crate's own legacy: `refs.redb`

Go imports the Pebble store its earlier releases kept. This crate's earlier
releases kept a redb database, `<dir>/refs.redb`, and `migrate.rs` imports
that, by the same design and with the same guarantees. `redb` stays a
dependency, used by `migrate.rs` only, until migration support is dropped.

- **One cheap check per open:** `refs.redb` absent, or a regular file that
  starts with the poison prefix — nothing to do. Anything else is a legacy
  database (or debris): take `<dir>/migrate.lock` (blocking
  `flock(LOCK_EX)`, EINTR retried; the lock file name Go uses) and look
  again under the lock, because another process may have finished meanwhile.
- **Open the redb database** first, in every case: that takes redb's own
  file lock, so a store an old binary still holds open fails with
  `Error::LegacyInUse` and is left exactly as it is. Then remove a crashed
  import's `refs.sqlite.tmp` and `refs.sqlite.tmp-journal`, whichever way
  the open goes on: after a crash between the link that publishes the
  database and the removal of its temporary name, that name is a second one
  of the LIVE database, which nothing must ever open
  (`crash_between_publishing_and_removing_the_temporary_name`).
- **Import**, when `refs.sqlite` does not exist: remove an orphaned
  `refs.sqlite-wal` / `refs.sqlite-shm` (see below; only here — next to a
  database that is there they are its live ones,
  `live_wal_of_an_existing_database_survives_the_merge`), build
  `refs.sqlite.tmp` with a rollback journal (`journal_mode=DELETE`) and full
  syncs, run the schema migrations in it, insert every record in ONE
  transaction, close it, and publish it: `link` it to `refs.sqlite`, remove
  the temporary name, fsync the directory. The link is **the commit
  point**: before it a crash leaves the redb store untouched; after it the
  legacy records are only ever merged in (next bullet), so later writes are
  never overwritten. `link(2)` fails where `rename(2)` would replace; on a
  filesystem without hard links the fallback is a look and a `rename`.
- **Merge**, when `refs.sqlite` exists — before the import, or by the time
  it wants to publish: open it the way the store does (WAL, schema
  migrations, full syncs) and insert the legacy records with `CreateRecord`
  (`ON CONFLICT DO NOTHING`) in one IMMEDIATE transaction. Names the
  database lacks are added; nothing in it is overwritten or removed. It is
  an ordinary writer and waits for another one
  (`merge_waits_for_a_foreign_write_transaction`). The directory is fsynced
  afterwards: whoever created the database may not have synced its entry,
  and the poison that follows must not outlive it. See "A database that is
  already there" below for why it is never trusted.
- **Retire**, still holding the redb database open and the migration lock:
  create `redb-migrated/`, hard-link `refs.redb` into it (or copy it, where
  the link cannot be made), fsync that directory, write the poison to `refs.redb.poison.tmp`, fsync it, `rename`
  it over `refs.redb`, fsync the directory. The rename replaces the path
  atomically: there is never a moment at which an old binary finds no file
  there and creates an empty database.
- A store that never had a redb file gets no poison, no lock file and no
  backup directory (`fresh_store_gets_no_poison`).

### A database that is already there

Go's rule — `refs.sqlite` next to legacy files means "imported", so the
import is never repeated — rests on the contract: every implementation
refuses a Pebble directory that has no `refs.sqlite`. Nothing makes Go refuse
a *redb* directory. Go 0.0.10 knows nothing of `refs.redb`, takes no
migration lock, and creates an empty `refs.sqlite` there. The first version
of this port copied Go's rule, and its review reproduced two ways to lose
every reference, with real binaries of both implementations:

- **Go runs first.** `amber-store-go ref list` on a redb-era store prints
  nothing and leaves an empty `refs.sqlite`. The next Rust open took that for
  a finished import, retired the redb store unread and put the poison in its
  place: no reference visible to either implementation, one gc run away from
  losing every object.
- **Go opens while the import builds its database** (1.6 s for 400,000
  references). The `rename` replaced Go's live file; Go's `-wal` and `-shm`
  stayed at the path and SQLite replayed them over the imported database.
  Afterwards both implementations saw Go's records only, `integrity_check`
  was fine, and `refs.redb` was already the poison.

Hence: publish with `link`, which replaces nothing, and merge into whatever
database is there instead of believing it
(`a_database_next_to_a_never_imported_store_does_not_hide_it`,
`merging_a_legacy_store_overwrites_nothing`, and in the unit tests, through a
hook between the finished database and its publication,
`a_database_appearing_during_the_import_is_kept_and_merged_into`). A crash
after the commit point comes back through the same path; the merge then
finds every name present and changes nothing
(`interrupted_cleanup_does_not_reimport`).

What this side cannot close:

- Go ran a gc cycle while the references were invisible to it. The merged
  references then name objects that are gone, and the next mark fails
  loudly. So: open a redb-era store with this crate first (README).
- A name deleted from `refs.sqlite` comes back if a legacy file still holds
  it: a crash in the few syscalls between the commit point and the poison,
  followed by a deletion through Go only; or an operator putting an old
  `refs.redb` back. Stale, but visible, and nothing is lost. A marker that
  told the import's own database from a foreign one would close the first
  case and was judged not worth another on-disk protocol element.
- Between the last look for `refs.sqlite` and the removal of an orphaned WAL
  lie a few microseconds in which a process that takes no migration lock
  could create both.

The real fix belongs in `architecture/references.md` and in Go: refuse a
directory that holds a `refs.redb` which is not the poison and no
`refs.sqlite`, the way a Pebble directory is refused here.

### Details worth knowing


- The backup link is skipped when `redb-migrated/refs.redb` already IS the
  legacy file (same device and inode): a retirement resumed after a crash
  between link and rename. If a *different* file has that name — the backup
  of an earlier migration, after a roll-back and a second upgrade — it is
  left alone and the new backup is linked as `refs.redb.1`, `.2`, …: the
  rename that follows removes the legacy database's last other name, so
  skipping the link there would destroy the only copy.
- **Where the link cannot be made** — `redb-migrated` on another filesystem,
  or a filesystem without hard links — the backup is a copy, written under a
  `.tmp` name, fsynced and renamed (`import_and_backup_without_hard_links`).
  Failing instead would fail every later open *after* the commit point, with
  no poison in place and old binaries free to write references that nobody
  imports. A copy cannot be recognised by inode when a retirement is
  resumed, so an earlier backup of the same length and content counts as
  this one's: a retirement that keeps failing after the backup (no room for
  the poison, say) does not add a full copy at every open
  (`a_retirement_that_keeps_failing_makes_one_backup_copy`). After a crash
  redb repairs the legacy file at the next open, the contents differ, and
  there is a second, numbered copy. A copy is also a crash image where a
  link is clean: redb sets "recovery required" in the header while a
  database is open and clears it on close, which lands in the linked backup
  but not in a copy made meanwhile. redb repairs such a backup when it is
  opened, which therefore needs writable media.
- **An orphaned WAL.** A crash can leave `refs.sqlite-wal` behind, and the
  documented way back to an older release is to delete `refs.sqlite`. The
  next upgrade's import then publishes a finished database next to that
  WAL. SQLite discards a stale WAL only when the database is empty;
  into this one it replays the old frames. Measured before the fix: the
  store opened with the first life's record and without the imported ones.
  The import runs under the migration lock with `refs.sqlite` just seen
  absent, so a `-wal` or `-shm` there belongs to nothing and is removed
  before the database is published
  (`orphaned_wal_is_not_replayed_into_the_import`). A store without a
  legacy file needs no such care: its new database is empty, and SQLite
  deletes the WAL itself.
- A zero-byte `refs.redb` (a crash at an old binary's very first open) is a
  legacy store without references: redb initialises it, there is no table,
  nothing is imported, and it is retired like any other.
- A `refs.redb` that is neither a redb database nor the poison fails the
  open with redb's error, and no `refs.sqlite` is created
  (`unreadable_legacy_file_fails_the_open`): an empty database there would
  make the directory look migrated. Next to a database that works it fails
  the open just the same, every time, and nothing is changed
  (`unreadable_legacy_file_next_to_a_database_fails_the_open`): whatever the
  file holds may never have been imported, so retiring it unread — what the
  first version did — is not this code's call. The operator moves it away.

## The poison

A small regular file at `refs.redb` that is NOT a redb database, so that a
binary from before this change fails in `redb::Database::create` instead of
silently creating an empty reference store, from which a gc run would reap
every object. It is the counterpart of Go's
`marker.format-version.999999.999`, which makes Pebble refuse the directory.

Layout (505 bytes):

| Offset | Content |
| --- | --- |
| 0 | redb's 9-byte magic number, `redb\x1A\x0A\xA9\x0D\x0A` |
| 9 | `\namber-store: references moved to refs.sqlite\n` — with the magic number, the 55-byte prefix the per-open check compares |
| 55 | `\n` padding up to 320 bytes, the size of redb's header |
| 64, 192 | `0xFF`: the file-format version byte of redb's two transaction slots |
| 320 | one explanatory sentence, ASCII |

**Why the magic number.** The brief for this port asked for a plain ASCII
file and to verify it empirically. The verification found that an ASCII file
only works from redb 2.4.0 on ("Fix `open()` and `create()` to return
`InvalidData` when they are called on a database file that is not a valid
redb database"): redb 2.0.0–2.3.0 take ANY file without the magic number for
a new database — whatever its size, so padding does not help — and overwrite
it. This crate has always locked redb 2.6.3, but it declares `redb = "2"`,
so a downstream binary may carry an older one. With the magic number in
place every 2.x release takes the existing-database path instead, reads the
header, and refuses the version byte.

How it was verified: a throwaway crate, rebuilt against each redb release,
wrote the candidates and called `Database::create` and `Database::open` on
them, catching panics and comparing the file's bytes afterwards.

| redb | plain ASCII sentence (also padded to 4 KiB, 64 KiB) | the poison above |
| --- | --- | --- |
| 2.0.0, 2.1.0–2.1.4, 2.2.0, 2.3.0 | **opened as a new database, file overwritten** | `DB corrupted: Expected file format version <= 2, found 255`; file unchanged |
| 2.4.0, 2.5.0 | `I/O error: invalid data`; file unchanged | same; file unchanged |
| 2.6.0–2.6.3 | `I/O error: invalid data`; file unchanged | `… version <= 3, found 255`; file unchanged |

A zero-byte file is taken for a new database by every release, so the poison
must never be empty. The kept tests pin the behaviour against the redb in
`Cargo.lock`: `poison_defeats_redb` and `poison_layout` (unit), and
`migrated_store_refuses_old_redb_binaries` on a really migrated directory.

## Pebble directories

This implementation cannot import Pebble. Per the rules document, when
`refs.sqlite` is absent and the directory holds a `marker.manifest.*` file,
`open` fails with `Error::PebbleStore` — the message says that the store has
to be opened once by the Go implementation, v0.0.10 or later, which imports
it — and creates nothing (`pebble_directory_is_refused` compares the
directory listing before and after). Next to Pebble files `refs.sqlite`
means "imported", and the store opens (`imported_pebble_directory_opens`).
The check runs BEFORE the redb import, because the import ends by creating
the very `refs.sqlite` that would tell Go its Pebble references had been
imported (`pebble_refusal_comes_before_the_redb_import`).

## Errors

`Error` keeps its shape: `NotFound`, new `Conflict` (`is_conflict()`),
`Open`, `Backend` (now a boxed `rusqlite::Error`), `NonUtf8Name`, plus what
the schema and the migration need. Messages Go also produces are Go's,
verbatim:

| Variant | Message |
| --- | --- |
| `NotFound` | `refstore: reference not found` |
| `Conflict` | `refstore: reference is not at the expected key` |
| `Open {context, source}` | `refstore: creating DIR: …`, `refstore: opening sqlite PATH: …`, `refstore: PATH: reading the journal mode: …` |
| `JournalMode` | `refstore: PATH: journal mode is "delete", want wal; the filesystem must support SQLite's shared-memory WAL index` |
| `Schema {path, source}` | the `SchemaError` followed by ` (PATH)`, Go's `%w (%s)` |
| `SchemaError::NotAReferenceStore` | `refstore: not a reference store: application_id is 0x0, want 0x616d6272` (`%#x` of a signed integer: `-0x1`) |
| `SchemaError::NotAVersion` | `refstore: schema version -1 is not a version` |
| `SchemaError::Newer` | `refstore: schema version 2 is newer than this release understands (1)` |
| `SchemaError::Migration` | `refstore: migration NAME: …` |
| `SchemaError::ReadState` / `Sqlite` / `Commit` | `refstore: reading the schema state: …` / `refstore: schema: …` / the commit error bare |
| `SchemaError::BadName` / `NoMigrations` / `NotContiguous` | `loadMigrations`' three messages |
| `CurrentRecord` | `refstore: current record of "NAME": …` |
| `Backend` | SQLite's error, unwrapped, as Go passes the driver's through |
| `Migrate {context, source}` | Go's import wraps with "redb" for "Pebble": `refstore: migration lock: …`, `refstore: migrating: …`, `refstore: opening the redb store PATH to migrate it: …`, `refstore: reading the redb store: …`, `refstore: retiring the redb store: …`, `refstore: syncing DIR: …` |
| `LegacyInUse`, `PebbleStore` | Rust only |

## Deviations from Go (and why)

1. **Legacy = redb, not Pebble; Pebble refused.** Decided for this port; see
   the two sections above. Consequences: the poison is a *file that replaces
   the database* rather than a marker next to it, the backup is a hard link
   (or a copy) and the retirement a rename-over rather than moving a set of
   files aside,
   because the legacy store is one file and its path must never be vacant.
2. **The poison starts with redb's magic number** rather than being plain
   ASCII, for the reason measured above.
3. **The busy timeout belongs to the store** (`Store::open_with`,
   crate-private) where Go's tests shorten a package variable: Rust's tests
   run in parallel threads of one process and would disturb one another.
4. **The reader pool caps idle connections at 8; Go's `maxConns` caps open
   ones**, so a ninth concurrent Go reader waits for a connection while a
   ninth Rust reader opens one and closes it afterwards. Reads never wait.
5. **The embedded migration set is validated at `open`** and a bad set is an
   `Error::Schema`; Go panics at package init. It is a build defect either
   way, pinned by `embedded_migrations_are_contiguous`.
6. **No `filepath.Abs`, no `file:` URI**: SQLite gets the path as it is,
   with `./` in front of a relative one that begins with `file:`.
7. **`Record.name` is a `String`.** Go's names are arbitrary bytes and the
   file is now shared, so `Error::NonUtf8Name`, unreachable while redb was
   private to this crate, can now happen: `all` fails loudly rather than
   mangle such a name, and other names stay reachable
   (`non_utf8_name_fails_all_loudly`). `reference::validate_name` forbids
   such names anyway. Likewise a record stored as TEXT — which the format
   forbids — is an error here, where Go's driver would hand its bytes over.
8. **A legacy file found next to `refs.sqlite` is opened and merged**, not
   just retired: see "A database that is already there". Opening it takes
   redb's `flock`, so an old binary's writes cannot vanish into the backup
   (`retirement_refuses_a_store_in_use`). Go resumes a retirement without
   looking at the legacy store, which its contract makes sound for Pebble.
   The database is published with `link`, not `rename`.
9. **Numbered backups** (`refs.redb.1`, …), see above.
10. **`Close()` is RAII**: dropping the store closes its connections, and
    SQLite checkpoints the WAL when the last connection to the file goes.
11. **The import removes an orphaned `refs.sqlite-wal` / `-shm`**, see
    above. Go's `importPebble` removes only its own temporaries and has the
    same exposure after a crash followed by a roll-back and a second
    upgrade: the review reproduced it against Go 91da3cf (the first life's
    record came back and the imported one was missing), and
    `architecture/references.md` does not tell the operator to delete
    `-wal` and `-shm` along with `refs.sqlite`.

## Tests

- `tests/refstore.rs` — the port of `refstore_test.go` and the batch tests —
  passes unchanged: it is the API contract.
- `src/refstore/tests.rs` (needs private parts; Go's `*_internal_test.go`):
  WAL mode and the pragmas of each `sync` value on writer and reader
  connections, the format pins, a writer completing while a reader holds a
  cursor with the reader's snapshot intact, open and reads not waiting for a
  writer (the lock-free fast path), the busy timeout, the reader pool, the
  migration runner (naming rule, upgrade through a test-only second
  migration, rollback of a failing one, newer and negative versions), the
  panic and early-exit rollbacks, Go's message texts, the poison, a failed
  reader not going back into the pool, the literal path, and — through two
  test-only switches in `migrate.rs` — a database that another process
  creates between the import's finished file and its publication, and the
  import and the backup on a filesystem without hard links.
- `tests/refstore_sqlite.rs` (`sqlite_test.go`): a second PROCESS sharing an
  open store, two handles, atomic batches across handles, odd names and
  large records, the odd directory name, a relative directory that looks
  like a `file:` URI (opened from a child process with a working directory
  of its own), foreign and garbage files, concurrent first opens across
  threads and processes.
- `tests/refstore_cas.rs` (`cas_test.go`): the three forms, the undecodable
  current record, one winner among concurrent swappers across threads and —
  released together by a gate file — across processes.
- `tests/refstore_migrate.rs` (`migrate_test.go`, re-thought): the import,
  crash points before and after the commit point, the resumed retirement, a
  store held open by an "old binary" (a live redb handle) before and after
  the commit point, concurrent first opens of a legacy store from threads
  and from processes, the poison defeating `redb::Database::create`, debris,
  an orphaned WAL, a database that another implementation put next to a
  never-imported store (empty, with records of its own, held open with a
  live WAL, in a write transaction), the temporary name left by a crash
  right after publishing, an unreadable legacy file next to a database, the
  Pebble refusal.
- Child processes re-execute the test binary (`current_exe()` with `--exact
  child_process --nocapture`); an environment variable switches that test
  into worker mode, and it is a no-op otherwise. Nothing shells out to cargo.
- The unit tests that asserted redb behaviour are gone: `open_twice_fails`
  (two handles on one directory now work: `two_handles_share_one_store`) and
  `durability_mapping` (now `sync_flag_selects_durability_pragmas`).

No golden vectors exist for this module: the database file is not
byte-reproducible. The format is pinned by `file_format_pins` and, live, by
`interop/check.sh`, where each CLI reads, lists and conditionally moves the
references the other one wrote into the same store directory.
