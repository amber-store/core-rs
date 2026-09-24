//! Persists reference records in a SQLite database: name bytes → CBOR record
//! bytes, stored verbatim. It is a dumb KV layer; record validation belongs
//! to the daemon and the [`crate::reference`] module (Go package
//! `refstore`).
//!
//! The database is `<dir>/refs.sqlite` in WAL mode, so any number of
//! processes may hold a store open at once: readers work from a snapshot and
//! never block, and writers — in this process or another — queue behind one
//! another (SQLite runs one write transaction at a time; a second writer
//! waits up to the busy timeout). The file format is shared with the Go
//! implementation and specified in `architecture/references.md`: the schema
//! is built by the numbered files in `migrations/`, byte-identical copies of
//! Go's, and the statements are Go's `queries.sql` as sqlc renders them. See
//! `port-notes/refstore.md`.

mod cas;
mod migrate;
mod queries;
mod schema;
mod sqlite;
#[cfg(test)]
mod tests;

use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use rusqlite::{Connection, OptionalExtension, Transaction, TransactionBehavior, params};

pub use schema::SchemaError;

use crate::reference;
use crate::tarexport::GoQuote;
use queries::{DELETE_ALL_RECORDS, DELETE_RECORD, GET_RECORD, LIST_RECORDS, PUT_RECORD};

/// A boxed error kept as a source.
type BoxError = Box<dyn std::error::Error + Send + Sync + 'static>;

/// Errors from the reference store. [`Error::NotFound`] and
/// [`Error::Conflict`] mirror Go's `ErrNotFound` and `ErrConflict` sentinels
/// (messages verbatim); match them with [`Error::is_not_found`] and
/// [`Error::is_conflict`] where Go code would use `errors.Is`.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// The name is absent, from [`Store::get`], [`Store::delete`] and the
    /// compare forms (Go: `ErrNotFound`).
    #[error("refstore: reference not found")]
    NotFound,
    /// The reference is not in the state an optimistic write expected: for
    /// [`Store::create`] it exists, for the compare forms it points at
    /// another key. Nothing was changed; the caller re-reads and decides
    /// (Go: `ErrConflict`).
    #[error("refstore: reference is not at the expected key")]
    Conflict,
    /// Opening the store failed before any record was touched: the
    /// directory could not be created or the database not be opened.
    /// `context` is Go's wrap text (`"refstore: creating DIR"`, `"refstore:
    /// opening sqlite PATH"`, `"refstore: PATH: reading the journal mode"`).
    #[error("{context}: {source}")]
    Open {
        /// What was being done.
        context: String,
        /// The underlying failure.
        #[source]
        source: BoxError,
    },
    /// The database did not end up in WAL mode, which the format requires.
    #[error(
        "refstore: {}: journal mode is {}, want wal; the filesystem must support SQLite's shared-memory WAL index",
        path.display(),
        GoQuote(mode.as_bytes())
    )]
    JournalMode {
        /// The database file.
        path: PathBuf,
        /// The journal mode SQLite reported.
        mode: String,
    },
    /// The file is not a reference store this release can use, or bringing
    /// its schema up to date failed (Go: the `migrateSchema` errors, wrapped
    /// as `"%w (%s)"` with the path).
    #[error("{source} ({})", path.display())]
    Schema {
        /// The database file.
        path: PathBuf,
        /// What is wrong with it.
        #[source]
        source: SchemaError,
    },
    /// Any other SQLite failure, passed through unwrapped, as Go passes the
    /// driver's errors through. Boxed to keep the enum small.
    #[error(transparent)]
    Backend(Box<rusqlite::Error>),
    /// The record a compare form found under `name` does not decode. It is
    /// an error, never a match (Go: `"refstore: current record of %q: %w"`).
    #[error("refstore: current record of {}: {source}", GoQuote(name.as_bytes()))]
    CurrentRecord {
        /// The reference's name.
        name: String,
        /// Why the stored record does not decode.
        #[source]
        source: reference::Error,
    },
    /// Importing or retiring the `refs.redb` store of an earlier release
    /// failed. `context` follows Go's Pebble import (`"refstore:
    /// migrating"`, `"refstore: migration lock"`, …).
    #[error("{context}: {source}")]
    Migrate {
        /// What was being done.
        context: String,
        /// The underlying failure.
        #[source]
        source: BoxError,
    },
    /// The `refs.redb` store of an earlier release is still held open by
    /// another process, so it was left exactly as it is: copying it
    /// mid-flight would lose whatever that process writes next.
    #[error(
        "refstore: {} is held open by another process, presumably an older release; it was left untouched and is migrated once that process has exited",
        path.display()
    )]
    LegacyInUse {
        /// The legacy database file.
        path: PathBuf,
    },
    /// The directory holds a Pebble reference store, written by the Go
    /// implementation before the shared format, and no `refs.sqlite`. This
    /// implementation cannot import Pebble, and creating an empty database
    /// here would hide the references — a gc run would then reap the objects
    /// they keep alive.
    #[error(
        "refstore: {} holds a Pebble reference store and no refs.sqlite; this implementation cannot import it: open the store once with the Go implementation (github.com/amber-store/core v0.0.10 or later), which does",
        dir.display()
    )]
    PebbleStore {
        /// The store directory.
        dir: PathBuf,
    },
    /// A stored name is not valid UTF-8. [`Store::put`] takes `&str`, so
    /// this implementation never writes one, and [`reference::validate_name`]
    /// forbids them; but the file is shared, and Go's `string` names are
    /// arbitrary bytes. [`Record::name`] is a `String`, so [`Store::all`]
    /// fails loudly rather than mangle such a name.
    #[error("refstore: stored name is not valid UTF-8")]
    NonUtf8Name,
}

impl Error {
    /// Whether this is the typed not-found error (Go:
    /// `errors.Is(err, ErrNotFound)`).
    #[must_use]
    pub fn is_not_found(&self) -> bool {
        matches!(self, Error::NotFound)
    }

    /// Whether this is the typed conflict error of the optimistic writes
    /// (Go: `errors.Is(err, ErrConflict)`).
    #[must_use]
    pub fn is_conflict(&self) -> bool {
        matches!(self, Error::Conflict)
    }
}

// Let `?` lift SQLite's errors into `Error::Backend`, the way Go returns the
// driver's errors unwrapped.
impl From<rusqlite::Error> for Error {
    fn from(e: rusqlite::Error) -> Self {
        Error::Backend(Box::new(e))
    }
}

/// One (name, record-bytes) pair from [`Store::all`] (Go: `Record`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Record {
    /// The reference name (the row's key).
    pub name: String,
    /// The record bytes, stored verbatim.
    pub data: Vec<u8>,
}

/// A SQLite-backed name → record map. It is safe for concurrent use, by
/// threads and by processes. Readers ([`Store::get`], [`Store::all`]) never
/// block; writes are serialized in-process by the writer mutex, so they queue
/// on the mutex rather than polling SQLite's file lock, and across processes
/// by SQLite itself.
///
/// A `rusqlite::Connection` is `Send` but not `Sync`, so the store keeps one
/// writer connection behind a mutex — Go's `writeMu` — and a small pool of
/// reader connections, so that a read never waits behind a writer that is
/// itself waiting for another process.
pub struct Store {
    path: PathBuf,
    sync: bool,
    busy_timeout: Duration,
    writer: Mutex<Connection>,
    readers: Mutex<Vec<Connection>>,
}

impl std::fmt::Debug for Store {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Store")
            .field("path", &self.path)
            .field("sync", &self.sync)
            .finish_non_exhaustive()
    }
}

impl Store {
    /// Opens (creating if missing) the refs database in `dir`. `sync`
    /// selects the write durability, matching the daemon's `--sync` flag
    /// (Go: `Open`). A `refs.redb` store left in `dir` by an earlier release
    /// of this crate is imported first; a Pebble store left by an earlier Go
    /// release is refused. See `migrate.rs`.
    pub fn open(dir: impl AsRef<Path>, sync: bool) -> Result<Store, Error> {
        Self::open_with(dir.as_ref(), sync, sqlite::BUSY_TIMEOUT)
    }

    /// [`Store::open`] with the busy timeout chosen by the caller (Go: tests
    /// shorten the package's `busyTimeout` variable).
    pub(crate) fn open_with(
        dir: &Path,
        sync: bool,
        busy_timeout: Duration,
    ) -> Result<Store, Error> {
        std::fs::create_dir_all(dir).map_err(|e| Error::Open {
            context: format!("refstore: creating {}", dir.display()),
            source: Box::new(e),
        })?;
        // Before anything is created: the import below ends by publishing a
        // refs.sqlite, and next to Pebble files that name means "imported".
        migrate::refuse_pebble(dir)?;
        migrate::migrate_legacy(dir, busy_timeout)?;
        let path = dir.join(sqlite::DB_FILE);
        let writer = sqlite::open_db(&path, sync, true, busy_timeout)?;
        Ok(Store {
            path,
            sync,
            busy_timeout,
            writer: Mutex::new(writer),
            readers: Mutex::new(Vec::new()),
        })
    }

    /// The writer connection; holding the guard is holding Go's `writeMu`.
    /// A panic inside a write poisons the mutex, but by then the
    /// transaction's drop has rolled it back, so the connection is clean and
    /// the next writer may have it.
    fn writer(&self) -> MutexGuard<'_, Connection> {
        self.writer.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Runs `f` on a reader connection: an idle one, or a new one. The pool
    /// mutex is held only to take and to return the connection, never while
    /// reading. At most [`sqlite::MAX_IDLE_READERS`] connections stay open
    /// for reuse, and one whose read failed is closed instead.
    fn read<T>(&self, f: impl FnOnce(&Connection) -> Result<T, Error>) -> Result<T, Error> {
        let idle = self
            .readers
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .pop();
        let conn =
            match idle {
                Some(conn) => conn,
                None => sqlite::connect(&self.path, self.sync, true, self.busy_timeout).map_err(
                    |e| Error::Open {
                        context: format!("refstore: opening sqlite {}", self.path.display()),
                        source: Box::new(e),
                    },
                )?,
            };
        let result = f(&conn);
        if result.is_ok() {
            let mut readers = self.readers.lock().unwrap_or_else(PoisonError::into_inner);
            if readers.len() < sqlite::MAX_IDLE_READERS {
                readers.push(conn);
            }
        }
        result
    }

    /// Stores `record` under `name`, overwriting unconditionally (Go: `Put`).
    pub fn put(&self, name: &str, record: &[u8]) -> Result<(), Error> {
        let conn = self.writer();
        conn.prepare_cached(PUT_RECORD)?
            .execute(params![name.as_bytes(), record])?;
        Ok(())
    }

    /// Publishes `records` atomically, using the configured write
    /// durability (Go: `PutBatch`). When names repeat, the last record wins.
    /// An empty batch changes nothing. Readers of [`Store::all`] see the
    /// complete old or new snapshot.
    pub fn put_batch(&self, records: &[Record]) -> Result<(), Error> {
        if records.is_empty() {
            return Ok(());
        }
        let conn = self.writer();
        // Dropped before the guard: an early return or a panic rolls back.
        let tx = Transaction::new_unchecked(&conn, TransactionBehavior::Immediate)?;
        {
            let mut put = tx.prepare_cached(PUT_RECORD)?;
            for record in records {
                put.execute(params![record.name.as_bytes(), record.data])?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    /// Removes every name in `names` atomically and returns how many rows went
    /// away. A name that is not there is not an error: a withdrawal the caller
    /// has already made, or one another process made first, leaves the store in
    /// the state the caller asked for. An empty batch changes nothing. No Go
    /// counterpart.
    pub fn delete_batch(&self, names: &[String]) -> Result<usize, Error> {
        if names.is_empty() {
            return Ok(0);
        }
        let conn = self.writer();
        // Dropped before the guard: an early return or a panic rolls back.
        let tx = Transaction::new_unchecked(&conn, TransactionBehavior::Immediate)?;
        let mut removed = 0;
        {
            let mut delete = tx.prepare_cached(DELETE_RECORD)?;
            for name in names {
                removed += delete.execute([name.as_bytes()])?;
            }
        }
        tx.commit()?;
        Ok(removed)
    }

    /// Publishes `records` and withdraws `names` in one commit, so a caller
    /// that replaces one set of references with another is never seen half way.
    /// Withdrawals are applied first, so a name in both ends up published. A
    /// missing name is not an error, as in [`Store::delete_batch`]. No Go
    /// counterpart.
    pub fn update_batch(&self, records: &[Record], names: &[String]) -> Result<(), Error> {
        if records.is_empty() && names.is_empty() {
            return Ok(());
        }
        let conn = self.writer();
        let tx = Transaction::new_unchecked(&conn, TransactionBehavior::Immediate)?;
        {
            let mut delete = tx.prepare_cached(DELETE_RECORD)?;
            for name in names {
                delete.execute([name.as_bytes()])?;
            }
        }
        {
            let mut put = tx.prepare_cached(PUT_RECORD)?;
            for record in records {
                put.execute(params![record.name.as_bytes(), record.data])?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    /// Returns the record stored under `name`, or [`Error::NotFound`] (Go:
    /// `Get`).
    pub fn get(&self, name: &str) -> Result<Vec<u8>, Error> {
        self.read(|conn| {
            Ok(conn
                .prepare_cached(GET_RECORD)?
                .query_row([name.as_bytes()], |row| row.get::<_, Vec<u8>>(0))
                .optional()?)
        })?
        .ok_or(Error::NotFound)
    }

    /// Removes `name`, or returns [`Error::NotFound`] if absent (Go:
    /// `Delete`). The check is the statement's own row count, so concurrent
    /// deletes of the same name — from any process — report
    /// [`Error::NotFound`] to all but one caller.
    pub fn delete(&self, name: &str) -> Result<(), Error> {
        let conn = self.writer();
        let n = conn
            .prepare_cached(DELETE_RECORD)?
            .execute([name.as_bytes()])?;
        if n == 0 {
            return Err(Error::NotFound);
        }
        Ok(())
    }

    /// Returns every record in lexicographic name order, from one snapshot
    /// (Go: `All`).
    pub fn all(&self) -> Result<Vec<Record>, Error> {
        self.read(|conn| {
            let mut stmt = conn.prepare_cached(LIST_RECORDS)?;
            let mut rows = stmt.query([])?;
            let mut recs = Vec::new();
            while let Some(row) = rows.next()? {
                let name =
                    String::from_utf8(row.get::<_, Vec<u8>>(0)?).map_err(|_| Error::NonUtf8Name)?;
                recs.push(Record {
                    name,
                    data: row.get(1)?,
                });
            }
            Ok(recs)
        })
    }

    /// Deletes every record — the store-wipe operation (Go: `Wipe`).
    pub fn wipe(&self) -> Result<(), Error> {
        let conn = self.writer();
        conn.prepare_cached(DELETE_ALL_RECORDS)?.execute([])?;
        Ok(())
    }
}

// Go's Close() has no explicit counterpart: dropping the Store closes its
// connections, and SQLite checkpoints the WAL when the last connection to
// the database goes.
