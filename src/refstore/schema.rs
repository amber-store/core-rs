//! The schema migrations and their runner (Go: `refstore/schema.go`).
//!
//! A migration is one released schema step. Files are named
//! `NNNN_description.sql`, numbered contiguously from 0001, hold plain SQL
//! (one or more statements) and never change once released. They run inside
//! a transaction, so statements that cannot (`VACUUM`, `PRAGMA
//! journal_mode`) or that do nothing there (`PRAGMA foreign_keys`) do not
//! belong in one. `PRAGMA user_version` records how many have been applied;
//! there is no other bookkeeping, so every implementation runs the same
//! files by the same rule. The files in `migrations/` are byte-identical
//! copies of Go's `refstore/migrations/`.

use rusqlite::{Connection, Transaction, TransactionBehavior};

use super::sqlite::APPLICATION_ID;
use crate::tarexport::GoQuote;

/// One released schema step (Go: `migration`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Migration {
    pub(super) version: i64,
    pub(super) name: String,
    pub(super) sql: String,
}

/// The embedded migration files, name and body (Go: `//go:embed
/// migrations/*.sql`). `include_str!` takes one literal path, so the list is
/// written out; a unit test compares it with the directory's contents.
pub(super) const EMBEDDED: &[(&str, &str)] =
    &[("0001_refs.sql", include_str!("migrations/0001_refs.sql"))];

/// A failure to bring the database's schema up to date, or a database that
/// is not a reference store this release can use. The messages are Go's.
#[derive(Debug, thiserror::Error)]
pub enum SchemaError {
    /// Reading the application id, the version or the object count failed.
    #[error("refstore: reading the schema state: {0}")]
    ReadState(#[source] Box<rusqlite::Error>),
    /// Beginning the transaction, setting the application id or setting the
    /// version failed.
    #[error("refstore: schema: {0}")]
    Sqlite(#[source] Box<rusqlite::Error>),
    /// The file is some other SQLite database: its application id is not the
    /// reference store's, and it is not a fresh database either (an id of 0
    /// on a non-empty file included).
    #[error(
        "refstore: not a reference store: application_id is {}, want {:#x}",
        go_hex(*application_id),
        APPLICATION_ID
    )]
    NotAReferenceStore {
        /// The application id found.
        application_id: i64,
    },
    /// `user_version` is negative.
    #[error("refstore: schema version {0} is not a version")]
    NotAVersion(i64),
    /// A newer release wrote the database.
    #[error("refstore: schema version {version} is newer than this release understands ({latest})")]
    Newer {
        /// The version found.
        version: i64,
        /// The newest version this release knows.
        latest: i64,
    },
    /// A migration's SQL failed; nothing of it was kept.
    #[error("refstore: migration {name}: {source}")]
    Migration {
        /// The migration's file name.
        name: String,
        /// SQLite's error.
        #[source]
        source: Box<rusqlite::Error>,
    },
    /// Committing the migration transaction failed (Go returns the commit
    /// error bare).
    #[error(transparent)]
    Commit(Box<rusqlite::Error>),
    /// A migration file's name breaks the naming rule.
    #[error("refstore: migration {} is not named NNNN_description.sql", GoQuote(.0.as_bytes()))]
    BadName(String),
    /// The migration set is empty.
    #[error("refstore: no migrations")]
    NoMigrations,
    /// The migration set has a gap, a duplicate, or does not start at 0001.
    #[error("refstore: migrations are not contiguous from 0001: {name} is in position {position}")]
    NotContiguous {
        /// The file found out of place.
        name: String,
        /// The 1-based position it sorted into.
        position: usize,
    },
}

/// Go's `%#x` of a signed integer: a sign, then the magnitude.
fn go_hex(v: i64) -> String {
    if v < 0 {
        format!("-{:#x}", v.unsigned_abs())
    } else {
        format!("{v:#x}")
    }
}

/// The embedded migration set, validated (Go: the package-level
/// `migrations`, which panics at init on a bad set; here the error surfaces
/// from `open`).
pub(super) fn migrations() -> Result<Vec<Migration>, SchemaError> {
    load_migrations(EMBEDDED)
}

/// Validates a set of `(file name, SQL)` pairs by the naming rule and sorts
/// it (Go: `loadMigrations`).
pub(super) fn load_migrations(files: &[(&str, &str)]) -> Result<Vec<Migration>, SchemaError> {
    let mut set = Vec::with_capacity(files.len());
    for &(name, body) in files {
        // Go: strings.Cut(name, "_"); without a separator the whole name is
        // the prefix and the file is refused.
        let (prefix, found) = match name.split_once('_') {
            Some((prefix, _)) => (prefix, true),
            None => (name, false),
        };
        // Go: strconv.Atoi — an optional sign, then decimal digits.
        let version = prefix.parse::<i64>();
        match version {
            Ok(version) if found && prefix.len() == 4 && version >= 1 && name.ends_with(".sql") => {
                set.push(Migration {
                    version,
                    name: name.to_string(),
                    sql: body.to_string(),
                });
            }
            _ => return Err(SchemaError::BadName(name.to_string())),
        }
    }
    if set.is_empty() {
        return Err(SchemaError::NoMigrations);
    }
    set.sort_by_key(|m| m.version); // stable, as Go's SortStableFunc
    for (i, m) in set.iter().enumerate() {
        if m.version != i as i64 + 1 {
            return Err(SchemaError::NotContiguous {
                name: m.name.clone(),
                position: i + 1,
            });
        }
    }
    Ok(set)
}

/// What decides whether a database is fresh, foreign, current or behind (Go:
/// `schemaState`).
#[derive(Debug, Clone, Copy)]
struct SchemaState {
    app_id: i64,
    version: i64,
    objects: i64,
}

fn read_schema_state(conn: &Connection) -> Result<SchemaState, SchemaError> {
    let read = |query: &str| {
        conn.query_row(query, [], |row| row.get::<_, i64>(0))
            .map_err(|e| SchemaError::ReadState(Box::new(e)))
    };
    Ok(SchemaState {
        app_id: read("PRAGMA application_id")?,
        version: read("PRAGMA user_version")?,
        objects: read("SELECT count(*) FROM sqlite_master")?,
    })
}

/// Brings the database on `conn` up to the newest migration in `set` (Go:
/// `migrateSchema`). An up-to-date database is recognized from a plain read,
/// without the write lock, so opening a store never waits for another
/// process's write transaction. Otherwise everything happens inside one
/// IMMEDIATE transaction: a crash leaves the old version, and of two
/// processes racing to initialize or upgrade a store the second finds
/// nothing left to do.
pub(super) fn migrate_schema(conn: &Connection, set: &[Migration]) -> Result<(), SchemaError> {
    let latest = set.len() as i64;
    let sqlite = |e: rusqlite::Error| SchemaError::Sqlite(Box::new(e));
    let st = read_schema_state(conn)?;
    if st.app_id == APPLICATION_ID && st.version == latest {
        return Ok(());
    }
    // Dropping the transaction rolls it back, on every early return below
    // and on a panic: the write lock must never outlive this function, or it
    // would wedge every writer in every process.
    let tx = Transaction::new_unchecked(conn, TransactionBehavior::Immediate).map_err(sqlite)?;
    let st = read_schema_state(&tx)?;
    if st.app_id == APPLICATION_ID && st.version == latest {
        return Ok(()); // another process got here first
    }
    if st.app_id == 0 && st.version == 0 && st.objects == 0 {
        // A fresh database.
        tx.execute_batch(&format!("PRAGMA application_id = {APPLICATION_ID}"))
            .map_err(sqlite)?;
    } else if st.app_id != APPLICATION_ID {
        return Err(SchemaError::NotAReferenceStore {
            application_id: st.app_id,
        });
    }
    if st.version < 0 {
        return Err(SchemaError::NotAVersion(st.version));
    }
    if st.version > latest {
        return Err(SchemaError::Newer {
            version: st.version,
            latest,
        });
    }
    for m in &set[st.version as usize..] {
        tx.execute_batch(&m.sql)
            .map_err(|e| SchemaError::Migration {
                name: m.name.clone(),
                source: Box::new(e),
            })?;
    }
    tx.execute_batch(&format!("PRAGMA user_version = {latest}"))
        .map_err(sqlite)?;
    tx.commit().map_err(|e| SchemaError::Commit(Box::new(e)))
}
