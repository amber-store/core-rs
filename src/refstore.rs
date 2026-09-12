//! Persists reference records in an embedded KV database: name bytes → CBOR
//! record bytes, stored verbatim. It is a dumb KV layer; record validation
//! belongs to the daemon and the [`crate::reference`] module (Go package
//! `refstore`).
//!
//! **Backend divergence (by design, see PORTING.md):** Go uses Pebble; this
//! port uses [redb], a single `refs.redb` file inside the store directory.
//! The public semantics are identical — name → record bytes verbatim, blind
//! overwrite, typed not-found, lexicographic iteration, sync flag — but a
//! `refs/` directory written by one implementation is not openable by the
//! other. See `port-notes/refstore.md`.

use std::path::Path;

use redb::{Database, Durability, TableDefinition};

/// The single redb table holding the name → record map.
const TABLE: TableDefinition<&[u8], &[u8]> = TableDefinition::new("refs");

/// The database file created inside the store directory.
const DB_FILE: &str = "refs.redb";

/// Errors from the reference store. The `NotFound` variant mirrors Go's
/// `ErrNotFound` sentinel (message verbatim); match it with
/// [`Error::is_not_found`] where Go code would use `errors.Is`.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// The name is absent, from [`Store::get`] and [`Store::delete`] (Go:
    /// `ErrNotFound`).
    #[error("refstore: reference not found")]
    NotFound,
    /// Opening the database failed (Go: `"refstore: opening pebble: %w"`,
    /// with the backend name adapted). Boxed: `redb::Error` is large.
    #[error("refstore: opening redb: {0}")]
    Open(#[source] Box<redb::Error>),
    /// Any other backend failure, passed through unwrapped, as Go passes
    /// Pebble errors through. Boxed: `redb::Error` is large.
    #[error(transparent)]
    Backend(Box<redb::Error>),
    /// A stored name is not valid UTF-8. Unreachable through this API
    /// ([`Store::put`] takes `&str`); kept because [`Record::name`] is a
    /// `String` while the DB key is raw bytes — Go's `string(it.Key())` is a
    /// lossless byte copy, Rust's `String` is not.
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
}

// Let `?` lift redb's per-operation error types into `Error::Backend`, the
// way Go returns Pebble errors unwrapped.
macro_rules! backend_from {
    ($($t:ty),* $(,)?) => {$(
        impl From<$t> for Error {
            fn from(e: $t) -> Self {
                Error::Backend(Box::new(e.into()))
            }
        }
    )*};
}
backend_from!(
    redb::Error,
    redb::TransactionError,
    redb::TableError,
    redb::StorageError,
    redb::CommitError,
);

/// Wraps an open-stage failure (Go: `"refstore: opening pebble: %w"`).
fn open_err(e: impl Into<redb::Error>) -> Error {
    Error::Open(Box::new(e.into()))
}

/// One (name, record-bytes) pair from [`Store::all`] (Go: `Record`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Record {
    /// The reference name (the DB key).
    pub name: String,
    /// The record bytes, stored verbatim.
    pub data: Vec<u8>,
}

/// A redb-backed name → record map. It is safe for concurrent use: readers
/// ([`Store::get`], [`Store::all`]) run on MVCC snapshots without blocking,
/// and writes ([`Store::put`], [`Store::delete`], [`Store::wipe`]) are
/// serialized by redb's single-writer transaction lock — which also makes
/// [`Store::delete`]'s existence check linearizable against other writers,
/// exactly what Go's `writeMu` provides over Pebble.
pub struct Store {
    db: Database,
    durability: Durability,
}

impl std::fmt::Debug for Store {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Store")
            .field("durability", &self.durability)
            .finish_non_exhaustive()
    }
}

impl Store {
    /// Opens (creating if missing) the refs DB at `dir`. `sync` selects the
    /// write durability, matching the daemon's `--sync` flag (Go: `Open`;
    /// `pebble.Sync`/`pebble.NoSync` ↦ redb `Durability::Immediate` /
    /// `Durability::Eventual` — see `port-notes/refstore.md`).
    pub fn open(dir: impl AsRef<Path>, sync: bool) -> Result<Store, Error> {
        let dir = dir.as_ref();
        // Pebble creates the directory as needed; do the same.
        std::fs::create_dir_all(dir).map_err(|e| open_err(redb::Error::from(e)))?;
        let db = Database::create(dir.join(DB_FILE)).map_err(open_err)?;
        let durability = if sync {
            Durability::Immediate
        } else {
            Durability::Eventual
        };
        // Create the table up front so every later transaction — including
        // read-only ones — finds it.
        let tx = db.begin_write().map_err(open_err)?;
        tx.open_table(TABLE).map_err(open_err)?;
        tx.commit().map_err(open_err)?;
        Ok(Store { db, durability })
    }

    /// Stores `record` under `name`, overwriting unconditionally (Go: `Put`).
    pub fn put(&self, name: &str, record: &[u8]) -> Result<(), Error> {
        let mut tx = self.db.begin_write()?;
        tx.set_durability(self.durability);
        {
            let mut table = tx.open_table(TABLE)?;
            table.insert(name.as_bytes(), record)?;
        }
        tx.commit()?;
        Ok(())
    }

    /// Publishes every record in one transaction using the configured durability.
    /// Repeated names use the last record. Empty batches are accepted.
    /// Readers of `all` see the complete old or new snapshot.
    pub fn put_batch(&self, records: &[Record]) -> Result<(), Error> {
        let mut tx = self.db.begin_write()?;
        tx.set_durability(self.durability);
        {
            let mut table = tx.open_table(TABLE)?;
            for record in records {
                table.insert(record.name.as_bytes(), record.data.as_slice())?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    /// Returns the record stored under `name`, or [`Error::NotFound`] (Go:
    /// `Get`).
    pub fn get(&self, name: &str) -> Result<Vec<u8>, Error> {
        let tx = self.db.begin_read()?;
        let table = tx.open_table(TABLE)?;
        match table.get(name.as_bytes())? {
            Some(guard) => Ok(guard.value().to_vec()),
            None => Err(Error::NotFound),
        }
    }

    /// Removes `name`, or returns [`Error::NotFound`] if absent (Go:
    /// `Delete`). The existence check and the delete happen in one write
    /// transaction, so concurrent deletes of the same name report
    /// [`Error::NotFound`] to all but one caller.
    pub fn delete(&self, name: &str) -> Result<(), Error> {
        let mut tx = self.db.begin_write()?;
        tx.set_durability(self.durability);
        let existed = {
            let mut table = tx.open_table(TABLE)?;
            table.remove(name.as_bytes())?.is_some()
        };
        if !existed {
            // Dropping the transaction aborts it; nothing was written.
            return Err(Error::NotFound);
        }
        tx.commit()?;
        Ok(())
    }

    /// Returns every record in lexicographic name order (Go: `All`).
    pub fn all(&self) -> Result<Vec<Record>, Error> {
        let tx = self.db.begin_read()?;
        let table = tx.open_table(TABLE)?;
        let mut recs = Vec::new();
        // redb iterates `&[u8]` keys in lexicographic byte order, matching
        // Pebble's iterator order.
        for item in table.range::<&[u8]>(..)? {
            let (k, v) = item?;
            let name = String::from_utf8(k.value().to_vec()).map_err(|_| Error::NonUtf8Name)?;
            recs.push(Record {
                name,
                data: v.value().to_vec(),
            });
        }
        Ok(recs)
    }

    /// Deletes every record — the store-wipe operation (Go: `Wipe`). All
    /// deletions commit atomically in one write transaction, mirroring Go's
    /// single batch commit; the store stays usable afterwards.
    pub fn wipe(&self) -> Result<(), Error> {
        let mut tx = self.db.begin_write()?;
        tx.set_durability(self.durability);
        {
            let mut table = tx.open_table(TABLE)?;
            table.retain(|_, _| false)?;
        }
        tx.commit()?;
        Ok(())
    }
}

// Go's Close() has no explicit counterpart: dropping the Store closes the
// database (redb flushes its allocator state on drop and releases the file
// lock).

#[cfg(test)]
mod tests {
    use super::*;

    // The Go refstore tests live in refstore_test.go and are ported to
    // tests/refstore.rs (they exercise the public API only). This module
    // keeps the backend-specific checks.

    fn tempdir() -> tempfile::TempDir {
        tempfile::tempdir().unwrap()
    }

    /// The sync flag maps to redb durability as documented.
    #[test]
    fn durability_mapping() {
        let dir = tempdir();
        let s = Store::open(dir.path().join("sync"), true).unwrap();
        assert!(matches!(s.durability, Durability::Immediate));
        let s = Store::open(dir.path().join("nosync"), false).unwrap();
        assert!(matches!(s.durability, Durability::Eventual));
    }

    /// Open creates the directory (and parents) as needed, like Pebble.
    #[test]
    fn open_creates_directory() {
        let dir = tempdir();
        let nested = dir.path().join("a").join("b").join("refs");
        let s = Store::open(&nested, false).unwrap();
        assert!(nested.join(DB_FILE).is_file());
        drop(s);
    }

    /// Opening the same directory twice concurrently fails: redb holds a
    /// file lock, like Pebble's LOCK file.
    #[test]
    fn open_twice_fails() {
        let dir = tempdir();
        let s = Store::open(dir.path(), false).unwrap();
        let err = Store::open(dir.path(), false).unwrap_err();
        assert!(
            matches!(&err, Error::Open(_)),
            "second open should fail with Open: {err}"
        );
        drop(s);
        // After the first store closes, opening succeeds again.
        Store::open(dir.path(), false).unwrap();
    }

    /// Names are raw bytes in the DB; the empty name is a valid key, and an
    /// empty record is a valid value (verified differentially: Pebble
    /// accepts both, and Go's string conversion round-trips the empty name).
    #[test]
    fn empty_name_and_empty_record_round_trip() {
        let dir = tempdir();
        let s = Store::open(dir.path(), false).unwrap();
        s.put("", b"v").unwrap();
        assert_eq!(s.get("").unwrap(), b"v");
        let recs = s.all().unwrap();
        assert_eq!(recs.len(), 1);
        assert_eq!(recs[0].name, "");
        s.delete("").unwrap();
        assert!(s.get("").unwrap_err().is_not_found());
        // Empty record value (Go: Put(name, nil) round-trips as empty).
        s.put("n", b"").unwrap();
        assert_eq!(s.get("n").unwrap(), b"");
    }
}
