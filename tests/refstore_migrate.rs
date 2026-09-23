//! What `open` does with the stores of earlier releases: Go's
//! `refstore/migrate_test.go` re-thought for this crate's own legacy, the
//! redb database `refs.redb` — the import, the crash points before and after
//! its commit point, the poison that keeps older binaries out — and the
//! refusal of a Pebble directory, which only the Go implementation imports.

use std::collections::BTreeMap;
use std::fs;
use std::os::unix::fs::MetadataExt;
use std::path::Path;
use std::process::{Command, Output};
use std::sync::Barrier;
use std::time::{Duration, Instant};

use amber_store_core::refstore::{Error, Store};

/// The one table of the redb store the releases before the shared format
/// kept (`src/refstore.rs` up to crate version 0.4.0).
const LEGACY_TABLE: redb::TableDefinition<&[u8], &[u8]> = redb::TableDefinition::new("refs");

/// How a poison file starts: redb's magic number, then the marker.
const POISON_PREFIX: &[u8] =
    b"redb\x1a\x0a\xa9\x0d\x0a\namber-store: references moved to refs.sqlite\n";

fn open(dir: &Path) -> Store {
    Store::open(dir, false).expect("open refstore")
}

/// Writes a redb reference store the way the previous release did (Go:
/// `legacyStore`): `Database::create`, the table created eagerly, one write
/// transaction per put.
fn legacy_store(dir: &Path, records: &[(&str, &str)]) {
    fs::create_dir_all(dir).unwrap();
    let db = redb::Database::create(dir.join("refs.redb")).unwrap();
    let tx = db.begin_write().unwrap();
    tx.open_table(LEGACY_TABLE).unwrap();
    tx.commit().unwrap();
    for (name, record) in records {
        legacy_put(&db, name, record);
    }
}

fn legacy_put(db: &redb::Database, name: &str, record: &str) {
    let tx = db.begin_write().unwrap();
    {
        let mut table = tx.open_table(LEGACY_TABLE).unwrap();
        table.insert(name.as_bytes(), record.as_bytes()).unwrap();
    }
    tx.commit().unwrap();
}

/// Everything in the redb database at `path`.
fn legacy_records(path: &Path) -> BTreeMap<String, String> {
    use redb::ReadableTable as _;
    let db = redb::Database::open(path).unwrap();
    let tx = db.begin_read().unwrap();
    let table = tx.open_table(LEGACY_TABLE).unwrap();
    table
        .iter()
        .unwrap()
        .map(|item| {
            let (k, v) = item.unwrap();
            (
                String::from_utf8(k.value().to_vec()).unwrap(),
                String::from_utf8(v.value().to_vec()).unwrap(),
            )
        })
        .collect()
}

fn map(records: &[(&str, &str)]) -> BTreeMap<String, String> {
    records
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect()
}

fn want_records(s: &Store, want: &[(&str, &str)]) {
    let got: BTreeMap<String, String> = s
        .all()
        .unwrap()
        .into_iter()
        .map(|r| (r.name, String::from_utf8(r.data).unwrap()))
        .collect();
    assert_eq!(got, map(want));
}

fn is_poison(path: &Path) -> bool {
    fs::symlink_metadata(path).is_ok_and(|m| m.is_file())
        && fs::read(path).unwrap().starts_with(POISON_PREFIX)
}

fn file_names(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().into_string().unwrap())
        .collect();
    names.sort();
    names
}

// The counterpart of Go TestMigratesLegacyPebbleStore.
#[test]
fn migrates_legacy_redb_store() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    let legacy = [("a/x", "1"), ("", "empty-name"), ("b", "2")];
    legacy_store(dir, &legacy);

    let s = Store::open(dir, false).unwrap();
    want_records(&s, &legacy);
    s.put("c/new", b"3").unwrap();
    drop(s);

    assert!(
        is_poison(&dir.join("refs.redb")),
        "refs.redb is not the poison"
    );
    assert_eq!(
        legacy_records(&dir.join("redb-migrated/refs.redb")),
        map(&legacy),
        "the redb database was not kept intact in redb-migrated/"
    );
    assert_eq!(
        file_names(dir),
        ["migrate.lock", "redb-migrated", "refs.redb", "refs.sqlite"],
        "temporaries were left behind"
    );
    // A second open imports nothing again.
    want_records(
        &open(dir),
        &[("a/x", "1"), ("", "empty-name"), ("b", "2"), ("c/new", "3")],
    );
}

/// A store that never had a redb file gets no poison, no lock file and no
/// backup directory.
#[test]
fn fresh_store_gets_no_poison() {
    let tmp = tempfile::tempdir().unwrap();
    let s = open(tmp.path());
    s.put("a", b"1").unwrap();
    drop(s);
    let names = file_names(tmp.path());
    assert!(
        names.iter().all(|n| n.starts_with("refs.sqlite")),
        "a fresh store holds {names:?}"
    );
}

// The counterpart of Go TestMigratedStoreRefusesOldPebbleBinaries. What an
// older binary calls is `redb::Database::create`; were it to succeed, that
// binary would see an empty reference store, and a gc run from it would reap
// every object.
#[test]
fn migrated_store_refuses_old_redb_binaries() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    legacy_store(dir, &[("a", "1")]);
    drop(open(dir));
    let poison = fs::read(dir.join("refs.redb")).unwrap();
    let attempt = redb::Database::create(dir.join("refs.redb")).map(|_| ());
    assert!(
        attempt.is_err(),
        "a redb-based binary can still open the migrated directory; it would see no references"
    );
    assert_eq!(
        fs::read(dir.join("refs.redb")).unwrap(),
        poison,
        "the attempt changed the poison"
    );
    want_records(&open(dir), &[("a", "1")]);
}

// The counterpart of Go TestEmptyLegacyStoreMigrates.
#[test]
fn empty_legacy_store_migrates() {
    let tmp = tempfile::tempdir().unwrap();
    legacy_store(tmp.path(), &[]);
    want_records(&open(tmp.path()), &[]);
    assert!(is_poison(&tmp.path().join("refs.redb")));
}

/// An empty `refs.redb` is what a crash at an old binary's very first open
/// leaves: a legacy store without references.
#[test]
fn zero_byte_legacy_file_migrates() {
    let tmp = tempfile::tempdir().unwrap();
    fs::write(tmp.path().join("refs.redb"), b"").unwrap();
    want_records(&open(tmp.path()), &[]);
    assert!(is_poison(&tmp.path().join("refs.redb")));
}

// The counterpart of Go TestInterruptedCleanupDoesNotReimport: a crash after
// the import committed (the link that publishes refs.sqlite) but before the
// redb file was retired leaves both side by side. The next open must finish
// the retirement without overwriting newer data with the old: it merges, and
// finds every name present.
#[test]
fn interrupted_cleanup_does_not_reimport() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    legacy_store(dir, &[("a", "old")]);
    let legacy_bytes = fs::read(dir.join("refs.redb")).unwrap();

    let s = Store::open(dir, false).unwrap();
    s.put("a", b"new").unwrap();
    drop(s);
    // The redb database is back, next to refs.sqlite.
    fs::write(dir.join("refs.redb"), &legacy_bytes).unwrap();

    want_records(&open(dir), &[("a", "new")]);
    assert!(
        is_poison(&dir.join("refs.redb")),
        "the resumed cleanup left the redb database in place"
    );
    // The earlier backup was not overwritten; this one got its own name.
    assert_eq!(
        file_names(&dir.join("redb-migrated")),
        ["refs.redb", "refs.redb.1"]
    );
    assert_eq!(
        legacy_records(&dir.join("redb-migrated/refs.redb.1")),
        map(&[("a", "old")])
    );
}

/// Go knows nothing of `refs.redb` and takes no migration lock: opened on a
/// directory of this crate's earlier releases, it creates an empty
/// `refs.sqlite` next to the redb store. That database is no proof of an
/// import. Taken for one, it would hide every reference behind the poison,
/// and a gc run would reap every object.
#[test]
fn a_database_next_to_a_never_imported_store_does_not_hide_it() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    drop(open(dir)); // the other implementation was here first
    legacy_store(dir, &[("a", "1"), ("b", "2")]);

    want_records(&open(dir), &[("a", "1"), ("b", "2")]);
    assert!(is_poison(&dir.join("refs.redb")));
    assert_eq!(
        legacy_records(&dir.join("redb-migrated/refs.redb")),
        map(&[("a", "1"), ("b", "2")])
    );
    want_records(&open(dir), &[("a", "1"), ("b", "2")]);
}

/// What the other implementation wrote meanwhile stays: the merge adds the
/// names the database lacks and overwrites nothing.
#[test]
fn merging_a_legacy_store_overwrites_nothing() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    {
        let s = open(dir);
        s.put("b", b"newer").unwrap();
        s.put("c", b"3").unwrap();
    }
    legacy_store(dir, &[("a", "1"), ("b", "old")]);

    want_records(&open(dir), &[("a", "1"), ("b", "newer"), ("c", "3")]);
    assert!(is_poison(&dir.join("refs.redb")));
}

/// Next to a database that is there, `refs.sqlite-wal` and `-shm` are its live
/// ones, not orphans: another handle — Go's, or a second store — may hold the
/// database open with committed frames in its WAL. Only the import, which
/// runs when there is no database, may remove them.
#[test]
fn live_wal_of_an_existing_database_survives_the_merge() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    let wal = dir.join("refs.sqlite-wal");
    let first = open(dir);
    first.put("w", b"in the wal").unwrap();
    assert!(fs::metadata(&wal).unwrap().len() > 0, "nothing in the WAL");
    let wal_inode = fs::metadata(&wal).unwrap().ino();
    legacy_store(dir, &[("a", "1")]);

    let second = open(dir);
    want_records(&second, &[("a", "1"), ("w", "in the wal")]);
    assert_eq!(
        fs::metadata(&wal).unwrap().ino(),
        wal_inode,
        "the WAL of a live database was replaced"
    );
    second.put("x", b"2").unwrap();
    want_records(&first, &[("a", "1"), ("w", "in the wal"), ("x", "2")]);
}

/// The merge is an ordinary writer: it waits for another one instead of
/// failing the open, and what that one wrote stays.
#[test]
fn merge_waits_for_a_foreign_write_transaction() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    drop(open(dir));
    legacy_store(dir, &[("a", "1"), ("b", "old")]);
    let conn = rusqlite::Connection::open(dir.join("refs.sqlite")).unwrap();
    conn.execute_batch("BEGIN IMMEDIATE; INSERT INTO refs VALUES (x'62', x'6e6577');")
        .unwrap();
    let holder = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(700));
        conn.execute_batch("COMMIT").unwrap();
        conn
    });
    let started = Instant::now();
    let s = open(dir);
    assert!(
        started.elapsed() >= Duration::from_millis(600),
        "the open did not wait for the other writer"
    );
    want_records(&s, &[("a", "1"), ("b", "new")]);
    drop(holder.join().unwrap());
}

/// A crash between the link that publishes the database and the removal of
/// its temporary name leaves that name behind: a second name of the LIVE
/// database, which nothing must ever open. The next open goes through the
/// merge, not the import, and has to take the name away all the same.
#[test]
fn crash_between_publishing_and_removing_the_temporary_name() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    legacy_store(dir, &[("a", "1")]);
    let legacy_bytes = fs::read(dir.join("refs.redb")).unwrap();
    drop(open(dir));
    // Back to the moment after the link: no backup, no poison, two names.
    fs::remove_dir_all(dir.join("redb-migrated")).unwrap();
    fs::write(dir.join("refs.redb"), &legacy_bytes).unwrap();
    fs::hard_link(dir.join("refs.sqlite"), dir.join("refs.sqlite.tmp")).unwrap();
    fs::write(dir.join("refs.sqlite.tmp-journal"), b"").unwrap();

    let s = open(dir);
    want_records(&s, &[("a", "1")]);
    assert_eq!(
        file_names(dir)
            .iter()
            .filter(|n| n.contains(".tmp"))
            .count(),
        0,
        "left behind: {:?}",
        file_names(dir)
    );
    assert_eq!(fs::metadata(dir.join("refs.sqlite")).unwrap().nlink(), 1);
    s.put("b", b"2").unwrap();
    drop(s);
    want_records(&open(dir), &[("a", "1"), ("b", "2")]);
}

/// A `refs.redb` that cannot be read, next to a database that works, fails
/// the open as it does without one, and changes nothing: whatever it holds
/// may never have been imported. Moving it away is the operator's call.
#[test]
fn unreadable_legacy_file_next_to_a_database_fails_the_open() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    open(dir).put("a", b"1").unwrap();
    let garbage = b"garbage ".repeat(1024);
    fs::write(dir.join("refs.redb"), &garbage).unwrap();
    for _ in 0..2 {
        let err = Store::open(dir, false).expect_err("open retired what it could not read");
        assert!(err.to_string().contains("to migrate it"), "{err}");
    }
    assert_eq!(fs::read(dir.join("refs.redb")).unwrap(), garbage);
    assert!(!dir.join("redb-migrated").exists());
    fs::remove_file(dir.join("refs.redb")).unwrap();
    want_records(&open(dir), &[("a", "1")]);
}

/// A crash between the backup link and the poison's rename leaves the redb
/// database under both names. The resumed retirement keeps the one backup.
#[test]
fn resumed_retirement_keeps_the_backup_link() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    legacy_store(dir, &[("a", "1")]);
    drop(open(dir));
    fs::remove_file(dir.join("refs.redb")).unwrap();
    fs::hard_link(dir.join("redb-migrated/refs.redb"), dir.join("refs.redb")).unwrap();

    want_records(&open(dir), &[("a", "1")]);
    assert!(is_poison(&dir.join("refs.redb")));
    assert_eq!(file_names(&dir.join("redb-migrated")), ["refs.redb"]);
    assert_eq!(
        legacy_records(&dir.join("redb-migrated/refs.redb")),
        map(&[("a", "1")])
    );
}

// The counterpart of Go TestMigrationRefusesAStoreInUse: a redb store that
// an old binary still has open cannot be migrated; it must be left exactly
// as it is.
#[test]
fn migration_refuses_a_store_in_use() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    legacy_store(dir, &[("a", "1")]);
    let in_use = redb::Database::create(dir.join("refs.redb")).unwrap(); // the "old binary"

    let err = Store::open(dir, false)
        .expect_err("open migrated a redb store that another owner holds open");
    assert!(matches!(err, Error::LegacyInUse { .. }), "got {err}");
    assert_eq!(
        file_names(dir),
        ["migrate.lock", "refs.redb"],
        "the refused migration left something behind"
    );
    legacy_put(&in_use, "b", "2"); // the old owner can still write
    drop(in_use);
    want_records(&open(dir), &[("a", "1"), ("b", "2")]);
}

/// The same after the commit point: a legacy file next to refs.sqlite that
/// an old binary holds open is not retired under its feet.
#[test]
fn retirement_refuses_a_store_in_use() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    legacy_store(dir, &[("a", "1")]);
    let legacy_bytes = fs::read(dir.join("refs.redb")).unwrap();
    drop(open(dir));
    fs::write(dir.join("refs.redb"), &legacy_bytes).unwrap();
    let in_use = redb::Database::create(dir.join("refs.redb")).unwrap();

    let err = Store::open(dir, false).expect_err("open retired a redb store in use");
    assert!(matches!(err, Error::LegacyInUse { .. }), "got {err}");
    assert!(!is_poison(&dir.join("refs.redb")));
    drop(in_use);
    want_records(&open(dir), &[("a", "1")]);
    assert!(is_poison(&dir.join("refs.redb")));
}

// The counterpart of Go TestStaleTemporaryDatabaseIsReplaced: a crash before
// the commit point leaves the import's temporaries and an untouched redb
// store.
#[test]
fn stale_temporaries_are_replaced() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    legacy_store(dir, &[("a", "1")]);
    for name in [
        "refs.sqlite.tmp",
        "refs.sqlite.tmp-journal",
        "refs.redb.poison.tmp",
    ] {
        fs::write(dir.join(name), b"left by a crashed import").unwrap();
    }
    want_records(&open(dir), &[("a", "1")]);
    assert_eq!(
        file_names(dir),
        ["migrate.lock", "redb-migrated", "refs.redb", "refs.sqlite"],
    );
    assert!(is_poison(&dir.join("refs.redb")));
}

/// A crash can leave `refs.sqlite-wal` behind, and the way back to an older
/// release is to delete `refs.sqlite` (architecture/references.md). The next
/// upgrade's import then publishes a finished database next to that orphaned
/// WAL. SQLite discards a stale WAL only for an empty database; into
/// this one it would replay the old frames, replacing what was just imported.
#[test]
fn orphaned_wal_is_not_replayed_into_the_import() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    let wal = dir.join("refs.sqlite-wal");
    {
        // A first life on SQLite. The copy is what a crash would leave: the
        // WAL as it is while the store is open, holding the committed record.
        let s = open(dir);
        s.put("from-the-first-life", b"x").unwrap();
        assert!(fs::metadata(&wal).unwrap().len() > 0, "nothing in the WAL");
        fs::copy(&wal, dir.join("wal.saved")).unwrap();
    }
    fs::rename(dir.join("wal.saved"), &wal).unwrap();
    // Back to the older release, which writes its redb store ...
    fs::remove_file(dir.join("refs.sqlite")).unwrap();
    legacy_store(dir, &[("a", "1")]);
    // ... and forward again.
    want_records(&open(dir), &[("a", "1")]);
    drop(open(dir));
    want_records(&open(dir), &[("a", "1")]);
}

// The counterpart of Go TestFilesThatAreNotPebblesStayPut.
#[test]
fn files_that_are_not_ours_stay_put() {
    let tmp = tempfile::tempdir().unwrap();
    legacy_store(tmp.path(), &[("a", "1")]);
    fs::write(tmp.path().join("NOTES.txt"), b"mine").unwrap();
    drop(open(tmp.path()));
    assert_eq!(fs::read(tmp.path().join("NOTES.txt")).unwrap(), b"mine");
}

/// A `refs.redb` that is neither a redb database nor the poison cannot be
/// imported. The open fails, and above all creates no empty `refs.sqlite`
/// that would make the directory look migrated.
#[test]
fn unreadable_legacy_file_fails_the_open() {
    let tmp = tempfile::tempdir().unwrap();
    fs::write(tmp.path().join("refs.redb"), b"garbage ".repeat(1024)).unwrap();
    let err = Store::open(tmp.path(), false).expect_err("open imported garbage");
    assert!(
        err.to_string().contains("to migrate it"),
        "the error does not say what was being done: {err}"
    );
    assert_eq!(file_names(tmp.path()), ["migrate.lock", "refs.redb"]);
}

// The counterpart of Go TestConcurrentOpensMigrateOnce.
#[test]
fn concurrent_opens_migrate_once() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    let want = [("a", "1"), ("b", "2")];
    legacy_store(dir, &want);
    const N: usize = 4;
    let gate = Barrier::new(N);
    let stores: Vec<Result<Store, Error>> = std::thread::scope(|scope| {
        let opens: Vec<_> = (0..N)
            .map(|_| {
                scope.spawn(|| {
                    gate.wait();
                    Store::open(dir, false)
                })
            })
            .collect();
        opens.into_iter().map(|h| h.join().unwrap()).collect()
    });
    for (i, s) in stores.iter().enumerate() {
        let s = s.as_ref().unwrap_or_else(|e| panic!("open {i}: {e}"));
        want_records(s, &want);
    }
    assert_eq!(file_names(&dir.join("redb-migrated")), ["refs.redb"]);
}

// ---------------------------------------------------------------------------
// The same between real processes.
// ---------------------------------------------------------------------------

const CHILD_DIR_ENV: &str = "REFSTORE_MIGRATE_CHILD_DIR";
const CHILD_NAME_ENV: &str = "REFSTORE_MIGRATE_CHILD_NAME";

/// One opener process: opens the store, puts its name. A no-op in a normal
/// test run.
#[test]
fn child_process() {
    let Some(dir) = std::env::var_os(CHILD_DIR_ENV) else {
        return;
    };
    let name = std::env::var(CHILD_NAME_ENV).unwrap();
    let verdict = Store::open(Path::new(&dir), false).and_then(|s| s.put(&name, b"v"));
    if let Err(e) = &verdict {
        eprintln!("child {name}: {e}");
    }
    std::process::exit(i32::from(verdict.is_err()));
}

#[test]
fn concurrent_first_opens_of_a_legacy_store_across_processes() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    legacy_store(dir, &[("a", "1"), ("b", "2")]);
    const N: usize = 6;
    let outs: Vec<Output> = std::thread::scope(|scope| {
        let children: Vec<_> = (0..N)
            .map(|i| {
                scope.spawn(move || {
                    Command::new(std::env::current_exe().expect("current_exe"))
                        .args(["--exact", "child_process", "--nocapture"])
                        .env(CHILD_DIR_ENV, dir)
                        .env(CHILD_NAME_ENV, format!("from-{i}"))
                        .output()
                        .expect("spawn the child test process")
                })
            })
            .collect();
        children.into_iter().map(|h| h.join().unwrap()).collect()
    });
    for (i, out) in outs.iter().enumerate() {
        assert!(
            out.status.success(),
            "child {i}: {:?}\n{}{}",
            out.status,
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
    }
    let s = open(dir);
    assert_eq!(s.all().unwrap().len(), 2 + N);
    assert_eq!(s.get("a").unwrap(), b"1");
    assert_eq!(file_names(&dir.join("redb-migrated")), ["refs.redb"]);
    assert!(is_poison(&dir.join("refs.redb")));
}

// ---------------------------------------------------------------------------
// Pebble directories (architecture/references.md, "Rules for implementations").
// ---------------------------------------------------------------------------

/// The files of a Pebble store, by name; this implementation looks at names
/// only.
fn pebble_directory(dir: &Path) {
    for name in [
        "000004.log",
        "CURRENT",
        "LOCK",
        "MANIFEST-000001",
        "OPTIONS-000003",
        "marker.format-version.000013.014",
        "marker.manifest.000001.MANIFEST-000001",
    ] {
        fs::write(dir.join(name), b"").unwrap();
    }
}

/// This implementation cannot import Pebble. It must refuse the directory,
/// and must never create an empty refs.sqlite there: it would hide the
/// references, and a gc run would reap the objects they keep alive.
#[test]
fn pebble_directory_is_refused() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    pebble_directory(dir);
    let before = file_names(dir);
    let err = Store::open(dir, false).expect_err("open accepted a Pebble directory");
    assert!(matches!(err, Error::PebbleStore { .. }), "got {err}");
    let msg = err.to_string();
    assert!(
        msg.contains("Pebble") && msg.contains("Go implementation") && msg.contains("v0.0.10"),
        "the error does not say what to do: {msg}"
    );
    assert_eq!(
        file_names(dir),
        before,
        "the refused open changed the directory"
    );
}

/// Next to Pebble files, refs.sqlite means "imported" (by Go): the store
/// opens.
#[test]
fn imported_pebble_directory_opens() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    open(dir).put("a", b"1").unwrap();
    pebble_directory(dir);
    want_records(&open(dir), &[("a", "1")]);
}

/// A directory both implementations used, each with its own format: the
/// Pebble refusal comes before the redb import, because the import ends by
/// creating the refs.sqlite that would tell Go its references were imported.
#[test]
fn pebble_refusal_comes_before_the_redb_import() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    legacy_store(dir, &[("a", "1")]);
    pebble_directory(dir);
    let before = file_names(dir);
    let err = Store::open(dir, false).expect_err("open accepted a Pebble directory");
    assert!(matches!(err, Error::PebbleStore { .. }), "got {err}");
    assert_eq!(file_names(dir), before);
    assert_eq!(legacy_records(&dir.join("refs.redb")), map(&[("a", "1")]));
}
