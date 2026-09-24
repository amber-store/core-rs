//! Unit tests of the SQLite backend: what needs the store's private parts
//! (Go: `sqlite_internal_test.go`, `schema_internal_test.go`,
//! `cas_internal_test.go`). The public-API tests live under `tests/`.

use std::cell::{Cell, RefCell};
use std::os::unix::fs::MetadataExt;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::Path;
use std::rc::Rc;
use std::sync::{Arc, Barrier};
use std::time::{Duration, Instant};

use rusqlite::Connection;

use super::schema::{self, Migration, SchemaError, load_migrations, migrate_schema};
use super::sqlite::{self, DB_FILE};
use super::*;
use crate::key::{Key, Type};
use crate::reference::Reference;

fn tempdir() -> tempfile::TempDir {
    tempfile::tempdir().unwrap()
}

fn open(dir: &Path) -> Store {
    Store::open(dir, false).unwrap()
}

/// Go shortens the package's `busyTimeout` for a test; here the timeout
/// belongs to the store, so parallel tests do not disturb one another.
fn open_with_timeout(dir: &Path, busy_timeout: Duration) -> Store {
    Store::open_with(dir, false, busy_timeout).unwrap()
}

/// A plain connection to the store's database, set up like the store's own
/// (Go: `rawDB`).
fn raw_db(dir: &Path, busy_timeout: Duration) -> Connection {
    sqlite::connect(&dir.join(DB_FILE), false, true, busy_timeout).unwrap()
}

fn pragma_int(conn: &Connection, name: &str) -> i64 {
    conn.query_row(&format!("PRAGMA {name}"), [], |row| row.get(0))
        .unwrap_or_else(|e| panic!("PRAGMA {name}: {e}"))
}

fn pragma_text(conn: &Connection, name: &str) -> String {
    conn.query_row(&format!("PRAGMA {name}"), [], |row| row.get(0))
        .unwrap_or_else(|e| panic!("PRAGMA {name}: {e}"))
}

/// An encoded reference called `name` pointing at `k`.
fn record(name: &str, k: Key, created_at: i64) -> Vec<u8> {
    Reference {
        name: name.to_string(),
        key: k.as_bytes().to_vec(),
        created_at,
        ..Default::default()
    }
    .encode()
    .unwrap()
}

/// Starts a write transaction on the store in `dir` and keeps it open until
/// the returned connection is dropped (Go: `holdWriteLock`).
fn hold_write_lock(dir: &Path) -> Connection {
    let conn = raw_db(dir, Duration::from_secs(5));
    conn.execute_batch(
        "BEGIN IMMEDIATE; INSERT INTO refs (name, record) VALUES (x'68656c64', x'');",
    )
    .unwrap();
    conn
}

/// `gc`, the CLI and the bench share one store between threads behind an
/// `Arc`.
#[test]
fn store_is_send_and_sync() {
    fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<Store>();
    assert_send_sync::<Error>();
}

/// Open creates the directory (and parents) as needed.
#[test]
fn open_creates_directory() {
    let dir = tempdir();
    let nested = dir.path().join("a").join("b").join("refs");
    let s = Store::open(&nested, false).unwrap();
    assert!(nested.join(DB_FILE).is_file());
    drop(s);
}

/// Names are raw bytes in the database; the empty name is a valid key, and
/// an empty record is a valid value. Both are bound as BLOBs, never NULL or
/// TEXT, as the shared format demands.
#[test]
fn empty_name_and_empty_record_round_trip() {
    let dir = tempdir();
    let s = open(dir.path());
    s.put("", b"v").unwrap();
    assert_eq!(s.get("").unwrap(), b"v");
    let recs = s.all().unwrap();
    assert_eq!(recs.len(), 1);
    assert_eq!(recs[0].name, "");
    s.delete("").unwrap();
    assert!(s.get("").unwrap_err().is_not_found());
    // Empty record value (Go: Put(name, nil) round-trips as empty).
    s.put("", b"").unwrap();
    assert_eq!(s.get("").unwrap(), b"");
    s.put_batch(&[Record {
        name: "batched".into(),
        data: Vec::new(),
    }])
    .unwrap();
    s.create("created", b"").unwrap();
    let raw = raw_db(dir.path(), Duration::from_secs(5));
    let mut stmt = raw
        .prepare("SELECT typeof(name), typeof(record), length(record) FROM refs")
        .unwrap();
    let types: Vec<(String, String, i64)> = stmt
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    assert_eq!(types.len(), 3);
    for t in types {
        assert_eq!(t, ("blob".to_string(), "blob".to_string(), 0));
    }
}

// Port of Go TestOpensInWALMode; every connection, not only the first.
#[test]
fn opens_in_wal_mode() {
    let dir = tempdir();
    let s = open(dir.path());
    assert_eq!(pragma_text(&s.writer(), "journal_mode"), "wal");
    let mode = s
        .read(|conn| Ok(pragma_text(conn, "journal_mode")))
        .unwrap();
    assert_eq!(mode, "wal");
}

// Port of Go TestSyncFlagSelectsDurabilityPragmas. The pragmas are per
// connection, so the reader connections are checked as well.
#[test]
fn sync_flag_selects_durability_pragmas() {
    for (sync, synchronous, full, checkpoint) in [(true, 2, 1, 1), (false, 1, 0, 0)] {
        let dir = tempdir();
        let s = Store::open(dir.path(), sync).unwrap();
        let check = |conn: &Connection, which: &str| {
            assert_eq!(
                pragma_int(conn, "synchronous"),
                synchronous,
                "sync={sync}, {which}: synchronous"
            );
            assert_eq!(
                pragma_int(conn, "fullfsync"),
                full,
                "sync={sync}, {which}: fullfsync"
            );
            assert_eq!(
                pragma_int(conn, "checkpoint_fullfsync"),
                checkpoint,
                "sync={sync}, {which}: checkpoint_fullfsync"
            );
            assert_eq!(
                pragma_int(conn, "busy_timeout"),
                30_000,
                "sync={sync}, {which}: busy_timeout"
            );
        };
        check(&s.writer(), "writer");
        s.read(|conn| {
            check(conn, "reader");
            Ok(())
        })
        .unwrap();
    }
}

// Port of Go TestFileFormatPins.
#[test]
fn file_format_pins() {
    let dir = tempdir();
    let s = open(dir.path());
    let conn = s.writer();
    assert_eq!(
        pragma_int(&conn, "application_id"),
        1_634_558_578,
        "application_id, want 0x616D6272"
    );
    assert_eq!(pragma_int(&conn, "user_version"), 1);
    let ddl: String = conn
        .query_row(
            "SELECT sql FROM sqlite_master WHERE name = 'refs'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(
        ddl.split_whitespace().collect::<Vec<_>>().join(" "),
        "CREATE TABLE refs ( name BLOB NOT NULL PRIMARY KEY, record BLOB NOT NULL ) WITHOUT ROWID"
    );
}

// Port of Go TestWriterIsNotBlockedByAnOpenReader. A reader holding a cursor
// open must not block a writer, and must keep its snapshot: that is WAL. In
// rollback-journal mode the put would wait out the busy timeout and fail.
#[test]
fn writer_is_not_blocked_by_an_open_reader() {
    let timeout = Duration::from_secs(2);
    let dir = tempdir();
    let first = open_with_timeout(dir.path(), timeout);
    let writer = open_with_timeout(dir.path(), timeout);
    for n in ["a", "b"] {
        first.put(n, n.as_bytes()).unwrap();
    }
    let reader = raw_db(dir.path(), timeout);
    let mut stmt = reader
        .prepare("SELECT name FROM refs ORDER BY name")
        .unwrap();
    let mut rows = stmt.query([]).unwrap();
    assert!(rows.next().unwrap().is_some(), "no first row");
    writer
        .put("c", b"c")
        .expect("a writer was blocked by an open reader");
    let mut seen = 1;
    while rows.next().unwrap().is_some() {
        seen += 1;
    }
    assert_eq!(seen, 2, "the reader must see its snapshot of 2 rows");
    assert_eq!(first.all().unwrap().len(), 3);
}

// Port of Go TestOpenAndReadDoNotWaitForAWriter: an up-to-date store is
// recognized from plain reads, without the write lock.
#[test]
fn open_and_read_do_not_wait_for_a_writer() {
    let timeout = Duration::from_secs(5);
    let dir = tempdir();
    let busy = open_with_timeout(dir.path(), timeout);
    busy.put("a", b"1").unwrap();
    let _held = hold_write_lock(dir.path());

    let start = Instant::now();
    let other = Store::open_with(dir.path(), false, timeout)
        .expect("open during another writer's transaction");
    assert_eq!(other.get("a").unwrap(), b"1");
    assert_eq!(other.all().unwrap().len(), 1);
    let took = start.elapsed();
    assert!(
        took < Duration::from_secs(2),
        "open+get+all took {took:?} while a writer was active; they must not wait for it"
    );
}

// Port of Go TestSecondWriterFailsAfterBusyTimeout.
#[test]
fn second_writer_fails_after_busy_timeout() {
    let timeout = Duration::from_millis(300);
    let dir = tempdir();
    let _busy = open_with_timeout(dir.path(), timeout);
    let other = open_with_timeout(dir.path(), timeout);
    let held = hold_write_lock(dir.path());

    let start = Instant::now();
    let err = other
        .put("b", b"2")
        .expect_err("put succeeded while another connection held the write lock");
    let took = start.elapsed();
    assert!(
        matches!(&err, Error::Backend(e) if sqlite::is_busy(e)),
        "want SQLite's busy error, got {err}"
    );
    assert!(
        took >= Duration::from_millis(200),
        "put gave up after {took:?}; the busy timeout was not applied"
    );
    assert!(
        took < Duration::from_secs(5),
        "put took {took:?} to give up, want about the busy timeout"
    );
    drop(held);
    other.put("b", b"2").expect("put after the writer finished");
}

/// Reads reuse an idle connection, and however many ran at once, at most
/// eight stay open afterwards.
#[test]
fn reader_pool_reuses_and_caps_idle_connections() {
    let dir = tempdir();
    let s = open(dir.path());
    for _ in 0..3 {
        assert!(s.get("missing").unwrap_err().is_not_found());
        assert!(s.all().unwrap().is_empty());
    }
    assert_eq!(
        s.readers.lock().unwrap().len(),
        1,
        "sequential reads reuse one connection"
    );

    const N: usize = 12;
    let gate = Barrier::new(N);
    std::thread::scope(|scope| {
        for _ in 0..N {
            scope.spawn(|| {
                s.read(|_| {
                    gate.wait(); // all N connections are checked out at once
                    Ok(())
                })
                .unwrap();
            });
        }
    });
    assert_eq!(s.readers.lock().unwrap().len(), sqlite::MAX_IDLE_READERS);
}

/// A reader never waits behind this process's writer, even one that is
/// itself waiting for another process.
#[test]
fn reads_do_not_wait_for_a_queued_writer() {
    let timeout = Duration::from_secs(3);
    let dir = tempdir();
    let s = Arc::new(open_with_timeout(dir.path(), timeout));
    s.put("a", b"1").unwrap();
    let held = hold_write_lock(dir.path());
    let writer = {
        let s = Arc::clone(&s);
        std::thread::spawn(move || s.put("b", b"2"))
    };
    std::thread::sleep(Duration::from_millis(100)); // the put now holds the writer mutex
    let start = Instant::now();
    assert_eq!(s.get("a").unwrap(), b"1");
    assert_eq!(s.all().unwrap().len(), 1);
    let took = start.elapsed();
    assert!(
        took < Duration::from_secs(1),
        "reads took {took:?} behind a waiting writer"
    );
    drop(held);
    writer.join().unwrap().expect("the queued put");
}

// ---------------------------------------------------------------------------
// Schema (Go: schema_internal_test.go).
// ---------------------------------------------------------------------------

// Port of Go TestEmbeddedMigrationsAreContiguous.
#[test]
fn embedded_migrations_are_contiguous() {
    let set = schema::migrations().unwrap();
    assert!(!set.is_empty(), "no embedded migrations");
    for (i, m) in set.iter().enumerate() {
        assert_eq!(m.version, i as i64 + 1, "migrations[{i}] = {}", m.name);
    }
}

/// `include_str!` takes literal paths, so the embedded list is written out
/// by hand; it must be exactly the directory's `.sql` files (Go embeds the
/// directory itself).
#[test]
fn embedded_migrations_match_the_directory() {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("src/refstore/migrations");
    let mut on_disk: Vec<(String, String)> = std::fs::read_dir(&dir)
        .unwrap()
        .map(|e| e.unwrap())
        .map(|e| {
            (
                e.file_name().into_string().unwrap(),
                std::fs::read_to_string(e.path()).unwrap(),
            )
        })
        .collect();
    on_disk.sort();
    let embedded: Vec<(String, String)> = schema::EMBEDDED
        .iter()
        .map(|&(name, sql)| (name.to_string(), sql.to_string()))
        .collect();
    assert_eq!(embedded, on_disk);
}

// Port of Go TestLoadMigrationsRejectsBadSets.
#[test]
fn load_migrations_rejects_bad_sets() {
    let bad: [(&str, &[&str]); 8] = [
        ("gap", &["0001_a.sql", "0003_c.sql"]),
        ("starts>1", &["0002_b.sql"]),
        ("duplicate", &["0001_a.sql", "0001_b.sql"]),
        ("bad name", &["0001_a.sql", "second.sql"]),
        ("short", &["1_a.sql"]),
        ("empty", &[]),
        ("not sql", &["0001_a.txt"]),
        ("zero", &["0000_a.sql"]),
    ];
    for (case, names) in bad {
        let files: Vec<(&str, &str)> = names.iter().map(|&n| (n, "SELECT 1;")).collect();
        assert!(
            load_migrations(&files).is_err(),
            "{case}: load_migrations accepted {names:?}"
        );
    }
    let good = [("0002_b.sql", "SELECT 2;"), ("0001_a.sql", "SELECT 1;")];
    let set = load_migrations(&good).unwrap();
    assert_eq!(set.len(), 2);
    assert_eq!(set[0].name, "0001_a.sql");
    assert_eq!(set[1].sql, "SELECT 2;");

    // Go's messages, verbatim.
    let msg = |names: &[&str]| {
        let files: Vec<(&str, &str)> = names.iter().map(|&n| (n, "SELECT 1;")).collect();
        load_migrations(&files).unwrap_err().to_string()
    };
    assert_eq!(
        msg(&["0001_a.sql", "second.sql"]),
        "refstore: migration \"second.sql\" is not named NNNN_description.sql"
    );
    assert_eq!(msg(&[]), "refstore: no migrations");
    assert_eq!(
        msg(&["0001_a.sql", "0003_c.sql"]),
        "refstore: migrations are not contiguous from 0001: 0003_c.sql is in position 2"
    );
}

/// The embedded set plus a test-only second migration (Go: `withSecond`).
fn with_second(sql: &str) -> Vec<Migration> {
    let mut set = schema::migrations().unwrap();
    set.push(Migration {
        version: set.len() as i64 + 1,
        name: "9999_test.sql".to_string(),
        sql: sql.to_string(),
    });
    set
}

// Port of Go TestMigrationsUpgradeAnExistingDatabase.
#[test]
fn migrations_upgrade_an_existing_database() {
    let dir = tempdir();
    let s = open(dir.path());
    s.put("a", b"1").unwrap();
    let conn = raw_db(dir.path(), Duration::from_secs(5));
    let next =
        with_second("ALTER TABLE refs ADD COLUMN note BLOB;\nUPDATE refs SET note = x'6e';\n");
    for _ in 0..2 {
        // the second run must find nothing to do
        migrate_schema(&conn, &next).unwrap();
    }
    assert_eq!(pragma_int(&conn, "user_version"), 2);
    let (note, rec): (Vec<u8>, Vec<u8>) = conn
        .query_row(
            "SELECT note, record FROM refs WHERE name = x'61'",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    assert_eq!((note.as_slice(), rec.as_slice()), (&b"n"[..], &b"1"[..]));
    // A store opened before the upgrade keeps working: migrations stay
    // compatible with the release before them.
    assert_eq!(s.get("a").unwrap(), b"1");
    s.put("b", b"2").unwrap();
}

// Port of Go TestFailedMigrationChangesNothing.
#[test]
fn failed_migration_changes_nothing() {
    let timeout = Duration::from_millis(500);
    let dir = tempdir();
    let s = open_with_timeout(dir.path(), timeout);
    s.put("a", b"1").unwrap();
    let conn = raw_db(dir.path(), timeout);
    let bad =
        with_second("ALTER TABLE refs ADD COLUMN note BLOB;\nINSERT INTO nowhere VALUES (1);\n");
    let err = migrate_schema(&conn, &bad).unwrap_err();
    assert!(
        matches!(&err, SchemaError::Migration { name, .. } if name == "9999_test.sql"),
        "want an error naming the migration, got {err}"
    );
    assert!(
        err.to_string()
            .starts_with("refstore: migration 9999_test.sql: ")
    );
    assert_eq!(pragma_int(&conn, "user_version"), 1);
    assert!(
        conn.prepare("SELECT note FROM refs").is_err(),
        "the failed migration's first statement was not rolled back"
    );
    assert_eq!(s.get("a").unwrap(), b"1");
    // The failed transaction is gone, its write lock with it.
    s.put("b", b"2")
        .expect("a failed migration left the write lock held");
}

// Port of Go TestNewerSchemaIsRefused.
#[test]
fn newer_schema_is_refused() {
    let dir = tempdir();
    let s = open(dir.path());
    s.writer().execute_batch("PRAGMA user_version = 2").unwrap();
    drop(s);
    let err = Store::open(dir.path(), false).unwrap_err();
    assert!(
        matches!(
            &err,
            Error::Schema {
                source: SchemaError::Newer {
                    version: 2,
                    latest: 1
                },
                ..
            }
        ),
        "got {err}"
    );
    assert_eq!(
        err.to_string(),
        format!(
            "refstore: schema version 2 is newer than this release understands (1) ({})",
            dir.path().join(DB_FILE).display()
        )
    );
}

// Port of Go TestNegativeSchemaVersionIsRefused. In Go a negative version
// used to panic inside the write transaction, and the unwinding then blocked
// forever with the write lock held.
#[test]
fn negative_schema_version_is_refused() {
    let dir = tempdir();
    let s = open(dir.path());
    s.writer()
        .execute_batch("PRAGMA user_version = -1")
        .unwrap();
    drop(s);
    let (tx, rx) = std::sync::mpsc::channel();
    let path = dir.path().to_path_buf();
    std::thread::spawn(move || {
        let _ = tx.send(Store::open(&path, false).map(|_| ()));
    });
    match rx.recv_timeout(Duration::from_secs(20)) {
        Ok(Err(err)) => assert_eq!(
            err.to_string(),
            format!(
                "refstore: schema version -1 is not a version ({})",
                dir.path().join(DB_FILE).display()
            )
        ),
        Ok(Ok(())) => panic!("a schema version of -1 was accepted"),
        Err(_) => panic!("open hangs on a negative schema version"),
    }
    // The refusal released the write lock.
    let conn = raw_db(dir.path(), Duration::from_millis(500));
    conn.execute_batch("BEGIN IMMEDIATE; ROLLBACK;")
        .expect("the refused open left the write lock held");
}

#[test]
fn application_id_renders_as_go_does() {
    let msg = |application_id| SchemaError::NotAReferenceStore { application_id }.to_string();
    assert_eq!(
        msg(0),
        "refstore: not a reference store: application_id is 0x0, want 0x616d6272"
    );
    assert_eq!(
        msg(42),
        "refstore: not a reference store: application_id is 0x2a, want 0x616d6272"
    );
    assert_eq!(
        msg(-1),
        "refstore: not a reference store: application_id is -0x1, want 0x616d6272"
    );
}

#[test]
fn error_messages_quote_as_go_does() {
    let err = Error::JournalMode {
        path: "/x/refs.sqlite".into(),
        mode: "delete".into(),
    };
    assert_eq!(
        err.to_string(),
        "refstore: /x/refs.sqlite: journal mode is \"delete\", want wal; the filesystem must support SQLite's shared-memory WAL index"
    );
    let dir = tempdir();
    let s = open(dir.path());
    s.put("a\"b\n", b"not cbor").unwrap();
    let k = Key::new(Type::Blob, 1, b"x");
    let err = s.compare_and_delete("a\"b\n", k).unwrap_err();
    assert!(
        err.to_string()
            .starts_with("refstore: current record of \"a\\\"b\\n\": "),
        "got {err}"
    );
}

// ---------------------------------------------------------------------------
// Optimistic writes (Go: cas_internal_test.go).
// ---------------------------------------------------------------------------

// Port of Go TestWriteTransactionsSurviveAPanic. A panic inside a write
// transaction must not leave SQLite's write lock held: a host that catches
// panics would otherwise wedge every writer in every process until it exits.
#[test]
fn write_transactions_survive_a_panic() {
    let timeout = Duration::from_millis(500);
    let dir = tempdir();
    let s = open_with_timeout(dir.path(), timeout);
    let other = open_with_timeout(dir.path(), timeout);
    let k = Key::new(Type::Blob, 1, b"x");
    s.put("r", &record("r", k, 1)).unwrap();
    let outcome = catch_unwind(AssertUnwindSafe(|| {
        s.if_at("r", k, |_, _| panic!("boom (expected by this test)"))
    }));
    assert!(outcome.is_err(), "the change did not panic");
    s.put("after", b"1")
        .expect("the handle that panicked cannot write any more");
    other
        .put("after-2", b"2")
        .expect("another handle cannot write any more");
}

/// Every refusal of a compare form ends its transaction: the next writer,
/// on another handle, does not wait.
#[test]
fn refused_compare_forms_release_the_write_lock() {
    let timeout = Duration::from_millis(500);
    let dir = tempdir();
    let s = open_with_timeout(dir.path(), timeout);
    let other = open_with_timeout(dir.path(), timeout);
    let (k1, k2) = (
        Key::new(Type::Blob, 3, b"one"),
        Key::new(Type::Blob, 3, b"two"),
    );
    s.put("r", &record("r", k1, 1)).unwrap();
    s.put("junk", b"not cbor").unwrap();

    assert!(
        s.compare_and_swap("absent", k1, b"x")
            .unwrap_err()
            .is_not_found()
    );
    other.put("w1", b"").expect("after a not-found");
    assert!(s.compare_and_swap("r", k2, b"x").unwrap_err().is_conflict());
    other.put("w2", b"").expect("after a conflict");
    assert!(matches!(
        s.compare_and_delete("junk", k1).unwrap_err(),
        Error::CurrentRecord { .. }
    ));
    other
        .put("w3", b"")
        .expect("after an undecodable current record");
    // The change itself touching no row is a conflict, too.
    assert!(s.if_at("r", k1, |_, _| Ok(0)).unwrap_err().is_conflict());
    other
        .put("w4", b"")
        .expect("after a change that touched nothing");
}

// ---------------------------------------------------------------------------
// The poison (migrate.rs).
// ---------------------------------------------------------------------------

/// redb's magic number, copied from its `header.rs`.
const REDB_MAGIC: [u8; 9] = [b'r', b'e', b'd', b'b', 0x1A, 0x0A, 0xA9, 0x0D, 0x0A];

#[test]
fn poison_layout() {
    let poison = migrate::poison();
    assert!(poison.starts_with(migrate::POISON_PREFIX));
    assert!(migrate::POISON_PREFIX.starts_with(&REDB_MAGIC));
    assert!(
        migrate::POISON_PREFIX.len() <= 64,
        "the prefix must end before redb's first transaction slot"
    );
    assert!(poison.len() > 320, "shorter than redb's header");
    assert_eq!((poison[64], poison[192]), (0xff, 0xff));
    let text = std::str::from_utf8(&poison[320..]).unwrap();
    assert!(
        text.is_ascii() && text.ends_with("redb-migrated/.\n"),
        "{text:?}"
    );
}

/// What the poison is for: redb must refuse the file instead of taking it
/// for a new database, and must leave it alone. `Database::create` is what
/// the releases before this change call. Checked here against the redb in
/// Cargo.lock; port-notes/refstore.md records the run against every 2.x
/// release, of which 2.0.0–2.3.0 overwrite a file without the magic number.
#[test]
fn poison_defeats_redb() {
    let dir = tempdir();
    let path = dir.path().join(migrate::LEGACY_FILE);
    let poison = migrate::poison();
    std::fs::write(&path, &poison).unwrap();
    let err = redb::Database::create(&path)
        .map(|_| ())
        .expect_err("redb opened the poison file: an old binary would see an empty store");
    assert!(
        err.to_string().contains("file format version"),
        "redb refused the poison, but not for its format version: {err}"
    );
    assert!(redb::Database::open(&path).is_err());
    assert_eq!(
        std::fs::read(&path).unwrap(),
        poison,
        "redb modified the poison"
    );
}

/// A reader connection whose read failed is closed, not pooled: whatever
/// state the failure left it in goes with it.
#[test]
fn a_reader_whose_read_failed_is_not_reused() {
    let dir = tempdir();
    let s = open(dir.path());
    assert!(s.all().unwrap().is_empty());
    assert_eq!(s.readers.lock().unwrap().len(), 1);
    let err = s
        .read(|_| -> Result<(), Error> { Err(Error::Conflict) })
        .unwrap_err();
    assert!(matches!(err, Error::Conflict), "got {err}");
    assert_eq!(
        s.readers.lock().unwrap().len(),
        0,
        "the connection of a failed read went back into the pool"
    );
}

/// The bundled SQLite parses any name that begins with `file:` as a URI,
/// whatever the open flags say; only a relative path can begin like that.
#[test]
fn a_path_that_looks_like_a_uri_is_taken_literally() {
    assert_eq!(
        &*sqlite::literal(Path::new("file:refs.sqlite?mode=ro")),
        Path::new("./file:refs.sqlite?mode=ro")
    );
    for plain in ["/abs/file:x", "refs.sqlite", "./file:x", "dir/file:x", ""] {
        assert_eq!(&*sqlite::literal(Path::new(plain)), Path::new(plain));
    }
}

// --- the redb import's private parts (migrate.rs); its public behaviour is
// --- tested in tests/refstore_migrate.rs

const LEGACY_TABLE: redb::TableDefinition<&[u8], &[u8]> = redb::TableDefinition::new("refs");

/// A redb reference store as the releases before the shared format left it.
fn legacy_store(dir: &Path, records: &[(&str, &str)]) {
    let db = redb::Database::create(dir.join(migrate::LEGACY_FILE)).unwrap();
    let tx = db.begin_write().unwrap();
    {
        let mut table = tx.open_table(LEGACY_TABLE).unwrap();
        for (name, record) in records {
            table.insert(name.as_bytes(), record.as_bytes()).unwrap();
        }
    }
    tx.commit().unwrap();
}

/// Everything in the redb database at `path`.
fn legacy_records(path: &Path) -> Vec<(String, String)> {
    use redb::ReadableTable as _;
    let db = redb::Database::open(path).unwrap();
    let tx = db.begin_read().unwrap();
    let table = tx.open_table(LEGACY_TABLE).unwrap();
    let text = |b: &[u8]| String::from_utf8(b.to_vec()).unwrap();
    table
        .iter()
        .unwrap()
        .map(|item| {
            let (k, v) = item.unwrap();
            (text(k.value()), text(v.value()))
        })
        .collect()
}

fn records_of(s: &Store) -> Vec<(String, String)> {
    s.all()
        .unwrap()
        .into_iter()
        .map(|r| (r.name, String::from_utf8(r.data).unwrap()))
        .collect()
}

fn pairs(want: &[(&str, &str)]) -> Vec<(String, String)> {
    want.iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect()
}

fn is_poison(path: &Path) -> bool {
    std::fs::read(path).is_ok_and(|b| b.starts_with(migrate::POISON_PREFIX))
}

fn file_names(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    names
}

/// Switches hard links off for the rest of a test, and clears both of
/// `migrate.rs`'s test switches when it goes: they are thread-locals, and a
/// harness that ran tests on one thread would carry them over.
struct NoHardLinks;

impl NoHardLinks {
    fn new() -> NoHardLinks {
        migrate::NO_HARD_LINKS.set(true);
        NoHardLinks
    }
}

impl Drop for NoHardLinks {
    fn drop(&mut self) {
        migrate::NO_HARD_LINKS.set(false);
        migrate::BEFORE_PUBLISH.set(None);
    }
}

/// What Go does in a directory whose `refs.redb` it knows nothing about, at
/// the worst moment: it creates `refs.sqlite`, writes to it and keeps it
/// open, just as the import is about to publish a database of its own. That
/// database must be neither replaced — the other process would go on writing
/// into an unlinked file, and SQLite would replay its WAL into the imported
/// one — nor trusted, which would hide every legacy reference.
fn database_appears_during_the_import(no_hard_links: bool) {
    let dir = tempdir();
    let path = dir.path().to_path_buf();
    legacy_store(&path, &[("a", "1"), ("z", "old")]);
    let _links = no_hard_links.then(NoHardLinks::new);

    let foreign: Rc<RefCell<Option<Connection>>> = Rc::default();
    let inode = Rc::new(Cell::new(0));
    let (held, created, at) = (Rc::clone(&foreign), Rc::clone(&inode), path.join(DB_FILE));
    migrate::BEFORE_PUBLISH.set(Some(Box::new(move || {
        let conn = sqlite::open_db(&at, false, true, Duration::from_secs(5)).unwrap();
        conn.execute(PUT_RECORD, params![b"z".as_slice(), b"new".as_slice()])
            .unwrap();
        created.set(std::fs::metadata(&at).unwrap().ino());
        held.replace(Some(conn));
    })));

    let s = open(&path);
    assert!(
        migrate::BEFORE_PUBLISH.take().is_none(),
        "the hook never ran"
    );
    assert_eq!(
        std::fs::metadata(path.join(DB_FILE)).unwrap().ino(),
        inode.get(),
        "the import replaced a database that another process had created"
    );
    assert_eq!(records_of(&s), pairs(&[("a", "1"), ("z", "new")]));
    let conn = foreign.take().expect("the hook kept its connection");
    let seen: i64 = conn
        .query_row("SELECT count(*) FROM refs", [], |row| row.get(0))
        .unwrap();
    assert_eq!(seen, 2, "the other process does not see the merged records");
    assert!(is_poison(&path.join(migrate::LEGACY_FILE)));
    assert!(!path.join(format!("{DB_FILE}.tmp")).exists());
}

#[test]
fn a_database_appearing_during_the_import_is_kept_and_merged_into() {
    database_appears_during_the_import(false);
}

#[test]
fn a_database_appearing_during_the_import_is_kept_without_hard_links_too() {
    database_appears_during_the_import(true);
}

/// A filesystem without hard links, or another one behind `redb-migrated`:
/// the database is published by a rename and the backup is a copy. Failing
/// the retirement instead would fail every later open after the commit
/// point, with no poison in place.
#[test]
fn import_and_backup_without_hard_links() {
    let dir = tempdir();
    legacy_store(dir.path(), &[("a", "1")]);
    let _links = NoHardLinks::new();

    assert_eq!(records_of(&open(dir.path())), pairs(&[("a", "1")]));
    assert!(is_poison(&dir.path().join(migrate::LEGACY_FILE)));
    let aside = dir.path().join(migrate::LEGACY_DIR);
    assert_eq!(file_names(&aside), ["refs.redb"]);
    assert_eq!(
        legacy_records(&aside.join("refs.redb")),
        pairs(&[("a", "1")])
    );
    assert!(!dir.path().join(format!("{DB_FILE}.tmp")).exists());
}

/// A backup that is a copy cannot be told by its inode when a retirement is
/// resumed. One that keeps failing after the backup — here a directory is in
/// the poison's way — must not add a full copy of the database at every open.
#[test]
fn a_retirement_that_keeps_failing_makes_one_backup_copy() {
    let dir = tempdir();
    legacy_store(dir.path(), &[("a", "1")]);
    let _links = NoHardLinks::new();
    let in_the_way = dir.path().join("refs.redb.poison.tmp");
    std::fs::create_dir(&in_the_way).unwrap();
    for _ in 0..3 {
        let err = Store::open(dir.path(), false).expect_err("retired without a poison");
        assert!(err.to_string().contains("retiring the redb store"), "{err}");
    }
    let aside = dir.path().join(migrate::LEGACY_DIR);
    assert_eq!(
        file_names(&aside),
        ["refs.redb"],
        "one copy per failed open"
    );

    std::fs::remove_dir(&in_the_way).unwrap();
    assert_eq!(records_of(&open(dir.path())), pairs(&[("a", "1")]));
    assert!(is_poison(&dir.path().join(migrate::LEGACY_FILE)));
    assert_eq!(file_names(&aside), ["refs.redb"]);
    assert_eq!(
        legacy_records(&aside.join("refs.redb")),
        pairs(&[("a", "1")])
    );
}

// ---------------------------------------------------------------------------
// Rust-only: delete_batch and update_batch.

fn rec(name: &str) -> Record {
    Record {
        name: name.to_string(),
        data: name.as_bytes().to_vec(),
    }
}

fn names(s: &Store) -> Vec<String> {
    s.all().unwrap().into_iter().map(|r| r.name).collect()
}

#[test]
fn delete_batch_counts_the_rows_it_removed() {
    let dir = tempdir();
    let s = open(dir.path());
    s.put_batch(&[rec("a"), rec("b"), rec("c")]).unwrap();

    // "z" is absent: not an error, and not counted.
    let removed = s
        .delete_batch(&["a".into(), "z".into(), "c".into()])
        .unwrap();
    assert_eq!(removed, 2);
    assert_eq!(names(&s), ["b"]);

    // Every name absent: still not an error.
    assert_eq!(s.delete_batch(&["a".into()]).unwrap(), 0);
    assert_eq!(s.delete_batch(&[]).unwrap(), 0);
    assert_eq!(names(&s), ["b"]);
}

#[test]
fn update_batch_withdraws_and_publishes_in_one_commit() {
    let dir = tempdir();
    let s = open(dir.path());
    s.put_batch(&[rec("old1"), rec("old2"), rec("kept")])
        .unwrap();

    s.update_batch(&[rec("new1")], &["old1".into(), "old2".into()])
        .unwrap();
    assert_eq!(names(&s), ["kept", "new1"]);
    assert_eq!(s.get("new1").unwrap(), b"new1");
}

/// The withdrawal runs first, so the caller gets the record rather than a hole.
#[test]
fn update_batch_publishes_a_name_it_also_withdraws() {
    let dir = tempdir();
    let s = open(dir.path());
    s.put("x", b"before").unwrap();
    s.update_batch(&[rec("x")], &["x".into()]).unwrap();
    assert_eq!(s.get("x").unwrap(), b"x");
}

#[test]
fn an_empty_update_batch_changes_nothing() {
    let dir = tempdir();
    let s = open(dir.path());
    s.put("x", b"v").unwrap();
    s.update_batch(&[], &[]).unwrap();
    assert_eq!(names(&s), ["x"]);
    assert_eq!(s.get("x").unwrap(), b"v");
}
