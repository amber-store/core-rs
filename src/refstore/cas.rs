//! The optimistic writes (Go: `refstore/cas.go`).

use rusqlite::{OptionalExtension, Transaction, TransactionBehavior, params};

use super::queries::{
    CREATE_RECORD, DELETE_RECORD, DELETE_RECORD_IF, GET_RECORD, PUT_RECORD, REPLACE_RECORD,
};
use super::{Error, Record, Store};
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

    /// Withdraws `names` and publishes `records` in one commit, each only if
    /// its reference is where the caller saw it, so a caller replacing one
    /// set of references with another is never seen half way and never
    /// undoes another process's move. A record's expectation is the key it
    /// replaces, or `None` for no reference; a withdrawn name may also be
    /// gone already, as the caller asked for that state. Every expectation
    /// is checked before anything changes, and if one fails nothing does and
    /// the result is [`Error::Conflict`]. Withdrawals run first, so a name in
    /// both ends up published. Returns how many rows the withdrawals removed
    /// (Go: `UpdateBatch`).
    pub fn update_batch(
        &self,
        records: &[(Record, Option<Key>)],
        names: &[(String, Key)],
    ) -> Result<usize, Error> {
        if records.is_empty() && names.is_empty() {
            return Ok(0);
        }
        let conn = self.writer();
        // Dropped before the guard: an early return or a panic rolls back.
        let tx = Transaction::new_unchecked(&conn, TransactionBehavior::Immediate)?;
        for (name, old) in names {
            if let Some((_, current)) = current(&tx, name)?
                && !points_at(&current, *old)
            {
                return Err(Error::Conflict);
            }
        }
        for (record, old) in records {
            let at = match (current(&tx, &record.name)?, old) {
                (None, None) => true,
                (Some((_, current)), Some(old)) => points_at(&current, *old),
                _ => false,
            };
            if !at {
                return Err(Error::Conflict);
            }
        }
        // The write lock has been held since the checks, so the plain
        // statements cannot undo another process's change.
        let mut removed = 0;
        {
            let mut delete = tx.prepare_cached(DELETE_RECORD)?;
            for (name, _) in names {
                removed += delete.execute([name.as_bytes()])?;
            }
        }
        {
            let mut put = tx.prepare_cached(PUT_RECORD)?;
            for (record, _) in records {
                put.execute(params![record.name.as_bytes(), record.data])?;
            }
        }
        tx.commit()?;
        Ok(removed)
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
        let (current, current_ref) = current(&tx, name)?.ok_or(Error::NotFound)?;
        if !points_at(&current_ref, old) {
            return Err(Error::Conflict);
        }
        if change(&tx, &current)? != 1 {
            return Err(Error::Conflict);
        }
        tx.commit()?;
        Ok(())
    }
}

/// The record stored under `name`, raw and decoded, or `None` if there is
/// none. A record that does not decode is an error.
fn current(tx: &Transaction<'_>, name: &str) -> Result<Option<(Vec<u8>, Reference)>, Error> {
    let Some(current) = tx
        .prepare_cached(GET_RECORD)?
        .query_row([name.as_bytes()], |row| row.get::<_, Vec<u8>>(0))
        .optional()?
    else {
        return Ok(None);
    };
    let decoded = Reference::decode(&current).map_err(|source| Error::CurrentRecord {
        name: name.to_string(),
        source,
    })?;
    Ok(Some((current, decoded)))
}

fn points_at(current: &Reference, key: Key) -> bool {
    current.key.as_slice() == key.as_bytes().as_slice()
}
