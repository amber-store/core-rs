# References

A **reference** is a global name pointing at a store key (a file, a
directory, or a [commit](commits.md)), recorded with its creator and creation time, with room for a
signature. References give roots names: `ingest --ref NAME` creates one, and
any `KEY[/PATH]` argument also accepts `ref:NAME[@PATH]`.

## The record

A reference is a canonical CBOR map (RFC 8949 §4.2 core-deterministic, the
same convention as fstree objects) with integer keys:

| CBOR key | Field | CBOR type | Notes |
| --- | --- | --- | --- |
| 0 | name | text string | global reference name |
| 1 | key | 32-byte byte string | pointed-to key, canonical per [keys.md](keys.md) |
| 2 | user | text string | creator identity; may be empty |
| 3 | created_at | int64 | ns since the Unix epoch |
| 4 | signature | byte string, omitted when absent | raw SSHSIG blob (see below) |
| 5 | public_key | byte string, omitted when absent | signer's public key, SSH wire format |

**Signing is a consumer concern.** The core stores keys 4 and 5 opaquely and
neither creates nor verifies signatures; the record format reserves them so
that signed records remain byte-compatible everywhere. The convention for
consumers that sign (the amber server and its clients): the **signature
payload** is the deterministic encoding of the record without key 4 — the
canonical bytes of `{0,1,2,3,5}` — so the signature covers the signer's
public key; key 4 holds an **SSHSIG v1** signature (the `ssh-keygen -Y sign`
format) over that payload, namespace `amber-store-ref`, SHA-512 message hash,
raw binary blob (not PEM-armored).

**Name rules:** 1–1024 bytes of valid UTF-8; no `@` (the ref/path separator)
and no control characters (< 0x20 or 0x7F). `/` is allowed
(`backups/2026/06`) but has no structural meaning — names are opaque strings,
compared whole.

**Field bounds:** the user string is limited to 1024 bytes (same character
rules as names, but `@` is allowed for email-style identities); a signature
may be at most 64 KiB. Decoders reject records whose bytes are not the
canonical deterministic encoding.

**Mutability:** references are overwritable; a plain put for an existing name
replaces the record unconditionally. There is no history. A writer that must
not overwrite somebody else's move uses the **optimistic** forms instead:
move the reference only if it still points at the key the writer last saw,
create it only if it does not exist, delete it only if it still points at a
given key. The comparison is on the pointed-to key, not on the whole record.
A failed expectation changes nothing and is reported to the caller, who
re-reads and decides.

## Storage

References live in a SQLite database (the `refstore` package), conventionally
`<store-dir>/refs/refs.sqlite` next to the object store. The file is the
interchange format: every implementation reads and writes the same database.

| Property | Value |
| --- | --- |
| `PRAGMA application_id` | `0x616D6272` (`"ambr"`) |
| `PRAGMA user_version` | number of schema migrations applied; `1` today |
| Journal mode | WAL, mandatory |
| Schema at version 1 | `CREATE TABLE refs (name BLOB NOT NULL PRIMARY KEY, record BLOB NOT NULL) WITHOUT ROWID` |

`name` is the reference name's bytes and `record` the CBOR record verbatim;
both are BLOBs, never NULL or TEXT, so listing (`ORDER BY name`) is bytewise
lexicographic. An implementation refuses a file whose application id differs
or whose version is newer than it knows.

The schema is built by numbered SQL files (`refstore/migrations/`), applied in
order inside one `BEGIN IMMEDIATE` transaction that ends by setting
`user_version`; a fresh database starts at version 0. Files never change once
released, and a change must stay compatible with the release before it, since
a process that opened the store earlier keeps running its old queries.

WAL mode makes the store multi-process: any number of processes may hold it
open, readers work from a snapshot and never block, and write transactions
run one at a time — a second writer waits (30 s busy timeout). A batch is one
transaction. Opening an up-to-date store takes no write lock, so it never
waits for a writer. One wrinkle: switching a new database into WAL mode takes
an exclusive lock for which SQLite does not run its busy handler, so an
implementation must retry a busy error while it connects, or concurrent
first opens fail. Write durability follows the store's sync flag:
`synchronous=FULL` with `fullfsync` and `checkpoint_fullfsync` on, or
`synchronous=NORMAL` without.

### Rules for implementations

An implementation that shares a store with others follows these; they are
what makes concurrent use by different programs safe.

- **Fresh or foreign.** A database with application id 0, version 0 and no
  schema objects is fresh: set the application id and apply the migrations,
  in one transaction. Any other database whose id is not `0x616D6272` — an
  id of 0 on a non-empty file included — is refused, and so is a negative
  version or one newer than the implementation knows.
- **Migrations** run inside that transaction, so a file holds no statement
  that cannot (`VACUUM`, `PRAGMA journal_mode`) or that does nothing there
  (`PRAGMA foreign_keys`).
- **Writes.** Every write transaction begins with `BEGIN IMMEDIATE`. A busy
  timeout is set on every connection before any other pragma. The switch to
  WAL can fail busy without the timeout applying; it is retried.
- **Optimistic forms.** The current record is read and decoded, its key
  compared, and the record replaced or deleted, all inside one write
  transaction. A reference that does not exist is *not found*; one that
  points elsewhere, or exists when it must not, is a *conflict*; a current
  record that does not decode is an error, never a match. The store retries
  nothing.
- **One host.** WAL shares memory between the processes that have the file
  open, so all of them run on one host; network filesystems are out.
- **Pebble directories.** `refs.sqlite` appears next to Pebble files only by
  the import's atomic rename, so its presence means "imported". An
  implementation that cannot import must refuse a directory that holds a
  `marker.manifest.*` file and no `refs.sqlite`, and must never create an
  empty `refs.sqlite` there: it would hide the references, and a GC run
  would reap the objects they keep alive.

**Stores written before this format** kept references in a Pebble DB in the
same directory. The first open imports them into `refs.sqlite`, moves the
Pebble files to `refs/pebble-migrated/` (a backup the operator may delete)
and leaves `marker.format-version.999999.999` behind. That marker makes
Pebble refuse the directory, so a binary that predates the change fails
loudly instead of creating an empty store — from which a `gc run` would reap
every object. `refs/migrate.lock`, a BSD `flock(2)` lock, serializes
concurrent first opens; a Pebble store that another process still holds open
is not migrated, and the open fails. To go back to a Pebble-based release,
move `pebble-migrated/*` back, delete the marker **and delete
`refs.sqlite`**: a later upgrade imports only when that file is absent, and
would otherwise keep the stale SQLite references and drop the newer Pebble
ones.

## CLI

```sh
amber-store ingest --ref NAME DIR    # ingest and name the root
amber-store ref set NAME KEY         # name an existing key
amber-store ref set --expect OLD NAME KEY   # only if NAME still points at OLD
amber-store ref set --expect none NAME KEY  # only if NAME does not exist
amber-store ref list                 # name, key, date, user
amber-store ref get NAME             # print the key NAME points at
amber-store ref rm NAME              # delete the name; objects stay
amber-store ref rm --expect OLD NAME # only if NAME still points at OLD
amber-store ls ref:NAME[@PATH]       # any KEY[/PATH] argument accepts this
```

`--expect` goes before the positional arguments.
