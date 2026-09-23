//! File name and format constants, the per-connection pragmas, the busy
//! retry around connecting, and the WAL check (Go: `refstore/sqlite.go`).

use std::borrow::Cow;
use std::os::unix::ffi::OsStrExt;
use std::path::Path;
use std::time::{Duration, Instant};

use rusqlite::{Connection, ErrorCode, OpenFlags};

use super::{Error, schema};

/// The database's name inside the store directory (Go: `dbFile`).
pub(super) const DB_FILE: &str = "refs.sqlite";

/// Marks the file as a reference store: "ambr" (Go: `applicationID`).
pub(super) const APPLICATION_ID: i64 = 0x616d_6272;

/// Idle reader connections kept for reuse; each is a file handle (Go:
/// `maxConns`, which caps the pool there).
pub(super) const MAX_IDLE_READERS: usize = 8;

/// How long a write waits for another connection's — usually another
/// process's — write transaction before failing with SQLite's busy error
/// (Go: `busyTimeout`, a variable so tests can shorten it; here tests pass
/// their own value to `Store::open_with`).
pub(super) const BUSY_TIMEOUT: Duration = Duration::from_secs(30);

/// `path` in a form SQLite does not parse. No connection passes
/// SQLITE_OPEN_URI, but the bundled library is compiled with SQLITE_USE_URI,
/// which makes it read any name that begins with `file:` as a URI all the
/// same. Only a relative path can begin like that, and `./` in front names
/// the same file.
pub(super) fn literal(path: &Path) -> Cow<'_, Path> {
    if path.as_os_str().as_bytes().starts_with(b"file:") {
        Cow::Owned(Path::new(".").join(path))
    } else {
        Cow::Borrowed(path)
    }
}

/// Opens one connection to the database at `path` and sets its pragmas (Go:
/// the DSN's `_pragma` list, which the driver applies to every connection of
/// the pool): the busy timeout first, then the journal mode, then the
/// durability the sync flag selects. `wal = false` selects a rollback
/// journal, used only for the single-file database the redb import builds.
fn connect_once(
    path: &Path,
    sync_writes: bool,
    wal: bool,
    busy_timeout: Duration,
) -> rusqlite::Result<Connection> {
    // The path is taken literally (see `literal`), so '?', '#', '%' and
    // spaces in it need no escaping (Go escapes them into a file: URI).
    // NO_MUTEX: a connection is only ever used by the one thread that holds
    // it, behind the writer mutex or checked out of the reader pool.
    let conn = Connection::open_with_flags(
        literal(path),
        OpenFlags::SQLITE_OPEN_READ_WRITE
            | OpenFlags::SQLITE_OPEN_CREATE
            | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )?;
    conn.busy_timeout(busy_timeout)?;
    let journal = if wal {
        "PRAGMA journal_mode=WAL"
    } else {
        "PRAGMA journal_mode=DELETE"
    };
    // The pragma answers with the resulting mode; `open_db` checks it.
    conn.query_row(journal, [], |row| row.get::<_, String>(0))?;
    if sync_writes {
        // fullfsync: on macOS a plain fsync does not reach the platter; the
        // packstore's File::sync_all already issues the full flush.
        conn.execute_batch(
            "PRAGMA synchronous=FULL; PRAGMA fullfsync=1; PRAGMA checkpoint_fullfsync=1;",
        )?;
    } else {
        conn.execute_batch("PRAGMA synchronous=NORMAL;")?;
    }
    Ok(conn)
}

/// Connects, retrying while SQLite reports the database busy (Go:
/// `connect`). Switching a database into WAL mode — which the first open of
/// a new or a freshly imported store does — takes an exclusive lock for
/// which SQLite does not run the busy handler: of several processes opening
/// such a store at once, all but one fail immediately. They retry here, for
/// as long as a writer would wait. Once the file is in WAL mode the pragma
/// changes nothing and never waits.
pub(super) fn connect(
    path: &Path,
    sync_writes: bool,
    wal: bool,
    busy_timeout: Duration,
) -> rusqlite::Result<Connection> {
    let deadline = Instant::now() + busy_timeout;
    let mut delay = Duration::from_millis(1);
    loop {
        match connect_once(path, sync_writes, wal, busy_timeout) {
            Err(e) if is_busy(&e) && Instant::now() <= deadline => {
                std::thread::sleep(delay);
                delay = (2 * delay).min(Duration::from_millis(50));
            }
            other => return other,
        }
    }
}

/// Whether `e` is SQLite's primary result code SQLITE_BUSY, extended codes
/// included (Go: `isBusy`).
pub(super) fn is_busy(e: &rusqlite::Error) -> bool {
    e.sqlite_error_code() == Some(ErrorCode::DatabaseBusy)
}

/// Opens the database at `path`, verifies WAL mode when asked for, and
/// brings the schema up to date (Go: `openDB` + `prepare`). The connection
/// it returns has done the store's first-open work.
pub(super) fn open_db(
    path: &Path,
    sync_writes: bool,
    wal: bool,
    busy_timeout: Duration,
) -> Result<Connection, Error> {
    let conn = connect(path, sync_writes, wal, busy_timeout).map_err(|e| Error::Open {
        context: format!("refstore: opening sqlite {}", path.display()),
        source: Box::new(e),
    })?;
    if wal {
        let mode = conn
            .query_row("PRAGMA journal_mode", [], |row| row.get::<_, String>(0))
            .map_err(|e| Error::Open {
                context: format!("refstore: {}: reading the journal mode", path.display()),
                source: Box::new(e),
            })?;
        if !mode.eq_ignore_ascii_case("wal") {
            return Err(Error::JournalMode {
                path: path.to_path_buf(),
                mode,
            });
        }
    }
    let set = schema::migrations().map_err(|source| Error::Schema {
        path: path.to_path_buf(),
        source,
    })?;
    schema::migrate_schema(&conn, &set).map_err(|source| Error::Schema {
        path: path.to_path_buf(),
        source,
    })?;
    Ok(conn)
}
