//! The optimistic writes (Go: `refstore/cas.go`).

use rusqlite::{OptionalExtension, Transaction, TransactionBehavior, params};

use super::queries::{CREATE_RECORD, DELETE_RECORD_IF, GET_RECORD, REPLACE_RECORD};
use super::{Error, Store};
use crate::key::Key;
use crate::reference::Reference;

impl Store {
    /// Stores `record` under `name` only if the reference currently points
    /// at `old` — the move from one key to the next that must not overwrite
    /// somebody else's move (Go: `CompareAndSwap`). It returns
    /// [`Error::NotFound`] if the reference does not exist and
    /// [`Error::Conflict`] if it points elsewhere. The expectation is the
    /// key, not the record bytes: a rewrite that kept the key does not
    /// invalidate it. `record` is stored verbatim, as by [`Store::put`].
    pub fn compare_and_swap(&self, name: &str, old: Key, record: &[u8]) -> Result<(), Error> {
        self.if_at(name, old, |tx, current| {
            Ok(tx.prepare_cached(REPLACE_RECORD)?.execute(params![
                record,
                name.as_bytes(),
                current
            ])?)
        })
    }

    /// Removes `name` only if it currently points at `old`, with
    /// [`Store::compare_and_swap`]'s errors (Go: `CompareAndDelete`).
    pub fn compare_and_delete(&self, name: &str, old: Key) -> Result<(), Error> {
        self.if_at(name, old, |tx, current| {
            Ok(tx
                .prepare_cached(DELETE_RECORD_IF)?
                .execute(params![name.as_bytes(), current])?)
        })
    }

    /// Stores `record` under `name` only if no such reference exists, and
    /// returns [`Error::Conflict`] if one does (Go: `Create`).
    pub fn create(&self, name: &str, record: &[u8]) -> Result<(), Error> {
        let conn = self.writer();
        let n = conn
            .prepare_cached(CREATE_RECORD)?
            .execute(params![name.as_bytes(), record])?;
        if n == 0 {
            return Err(Error::Conflict);
        }
        Ok(())
    }

    /// Runs `change` inside one write transaction after checking that `name`
    /// exists and points at `old` (Go: `ifAt`). The key lives inside the
    /// record, so the current record is decoded; one that does not decode is
    /// an error, never a match. `change` is guarded by the bytes just read
    /// and reports the rows it touched.
    pub(super) fn if_at(
        &self,
        name: &str,
        old: Key,
        change: impl FnOnce(&Transaction<'_>, &[u8]) -> Result<usize, Error>,
    ) -> Result<(), Error> {
        let conn = self.writer();
        // Declared after the guard, so dropped before it: every early return
        // below — and a panic in `change` — rolls the transaction back while
        // the connection is still ours. SQLite's write lock must never
        // outlive this call, or it would wedge every writer in every process.
        let tx = Transaction::new_unchecked(&conn, TransactionBehavior::Immediate)?;
        let current = tx
            .prepare_cached(GET_RECORD)?
            .query_row([name.as_bytes()], |row| row.get::<_, Vec<u8>>(0))
            .optional()?
            .ok_or(Error::NotFound)?;
        let current_ref = Reference::decode(&current).map_err(|source| Error::CurrentRecord {
            name: name.to_string(),
            source,
        })?;
        if current_ref.key.as_slice() != old.as_bytes().as_slice() {
            return Err(Error::Conflict);
        }
        if change(&tx, &current)? != 1 {
            return Err(Error::Conflict);
        }
        tx.commit()?;
        Ok(())
    }
}
