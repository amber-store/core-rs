//! Port of Go `refstore/sqlite_test.go`: the SQLite backend through the
//! public API — several handles, several threads and several PROCESSES on
//! one store directory, and the files `open` must refuse.
//!
//! Go's `TestMain` doubles as the second process. Here the test binary
//! re-executes itself running only [`child_process`], which an environment
//! variable switches into worker mode.

use std::path::Path;
use std::process::{Command, Output};
use std::sync::Barrier;

use amber_store_core::refstore::{Error, Record, SchemaError, Store};

const CHILD_DIR_ENV: &str = "REFSTORE_TEST_CHILD_DIR";
/// Set: the child only opens the store and puts this name.
const CHILD_NAME_ENV: &str = "REFSTORE_TEST_CHILD_NAME";

fn open(dir: &Path) -> Store {
    Store::open(dir, false).expect("open refstore")
}

/// The second process. A no-op in a normal test run; with [`CHILD_DIR_ENV`]
/// set it opens that store, does its part and exits with the verdict.
#[test]
fn child_process() {
    let Some(dir) = std::env::var_os(CHILD_DIR_ENV) else {
        return;
    };
    let verdict = run_child(Path::new(&dir));
    if let Err(e) = &verdict {
        eprintln!("child: {e}");
    }
    std::process::exit(i32::from(verdict.is_err()));
}

fn run_child(dir: &Path) -> Result<(), String> {
    let s = Store::open(dir, false).map_err(|e| e.to_string())?;
    if let Ok(name) = std::env::var(CHILD_NAME_ENV) {
        return s.put(&name, b"v").map_err(|e| e.to_string());
    }
    let got = s
        .get("from-parent")
        .map_err(|e| format!("reading the parent's record: {e}"))?;
    if got != b"p" {
        return Err(format!("from-parent = {got:?}, want p"));
    }
    s.put("from-child", b"c").map_err(|e| e.to_string())
}

fn spawn_child(dir: &Path, name: Option<&str>) -> Output {
    let mut cmd = Command::new(std::env::current_exe().expect("current_exe"));
    cmd.args(["--exact", "child_process", "--nocapture"])
        .env(CHILD_DIR_ENV, dir);
    if let Some(name) = name {
        cmd.env(CHILD_NAME_ENV, name);
    }
    cmd.output().expect("spawn the child test process")
}

fn assert_child_ok(out: &Output, what: &str) {
    assert!(
        out.status.success(),
        "{what}: {:?}\n{}{}",
        out.status,
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
}

// Port of Go TestSecondProcessSharesTheStore.
#[test]
fn second_process_shares_the_store() {
    let dir = tempfile::tempdir().unwrap();
    let s = open(dir.path()); // stays open for the child's whole life
    s.put("from-parent", b"p").unwrap();
    assert_child_ok(&spawn_child(dir.path(), None), "child process");
    assert_eq!(s.get("from-child").unwrap(), b"c");
}

// Port of Go TestPutNilRecordReadsBackEmpty.
#[test]
fn put_empty_record_reads_back_empty() {
    let dir = tempfile::tempdir().unwrap();
    let s = open(dir.path());
    s.put("nil", &[]).unwrap();
    assert_eq!(s.get("nil").unwrap(), b"");
}

// Port of Go TestTwoHandlesShareOneStore.
#[test]
fn two_handles_share_one_store() {
    let dir = tempfile::tempdir().unwrap();
    let (a, b) = (open(dir.path()), open(dir.path()));
    a.put("x", b"1").unwrap();
    assert_eq!(b.get("x").unwrap(), b"1");
    b.delete("x").unwrap();
    let err = a.get("x").unwrap_err();
    assert!(
        err.is_not_found(),
        "first handle after the second's delete: {err}, want NotFound"
    );
}

// Port of Go TestTwoHandlesKeepBatchesAtomic.
#[test]
fn two_handles_keep_batches_atomic() {
    let dir = tempfile::tempdir().unwrap();
    let handles = [open(dir.path()), open(dir.path())];
    std::thread::scope(|scope| {
        for worker in 0..4usize {
            let handles = &handles;
            scope.spawn(move || {
                for generation in 0..50 {
                    let value = format!("{worker}/{generation}").into_bytes();
                    let batch = ["a", "b"].map(|name| Record {
                        name: name.to_string(),
                        data: value.clone(),
                    });
                    handles[worker % 2].put_batch(&batch).unwrap();
                    let all = handles[(worker + 1) % 2].all().unwrap();
                    assert!(
                        all.len() == 2 && all[0].data == all[1].data,
                        "partial batch became visible"
                    );
                }
            });
        }
    });
}

// Port of Go TestArbitraryNamesAndLargeRecords. Go's names are arbitrary
// bytes; this API takes `&str`, so the odd names here are the odd ones UTF-8
// allows (the non-UTF-8 case is the next test).
#[test]
fn arbitrary_names_and_large_records() {
    let dir = tempfile::tempdir().unwrap();
    let s = open(dir.path());
    let big = vec![0xab_u8; 100 << 10];
    for n in ["a", "a\0b", "\u{10ffff}", "b", ""] {
        s.put(n, &big).unwrap_or_else(|e| panic!("put({n:?}): {e}"));
    }
    let all = s.all().unwrap();
    let want = ["", "a", "a\0b", "b", "\u{10ffff}"];
    assert_eq!(all.len(), want.len());
    for (rec, want) in all.iter().zip(want) {
        assert_eq!(rec.name, want);
        assert!(rec.data == big, "{want:?}: the record did not round-trip");
    }
}

/// The file is shared with Go, whose names are arbitrary bytes. `Record`
/// carries a `String`, so a name that is not UTF-8 fails `all` loudly; other
/// names stay reachable.
#[test]
fn non_utf8_name_fails_all_loudly() {
    let dir = tempfile::tempdir().unwrap();
    let s = open(dir.path());
    s.put("fine", b"1").unwrap();
    let raw = rusqlite::Connection::open(dir.path().join("refs.sqlite")).unwrap();
    raw.execute(
        "INSERT INTO refs (name, record) VALUES (x'fffe', x'02')",
        [],
    )
    .unwrap();
    let err = s.all().unwrap_err();
    assert!(matches!(err, Error::NonUtf8Name), "got {err}");
    assert_eq!(s.get("fine").unwrap(), b"1");
}

// Port of Go TestOpenPathWithSpecialCharacters.
#[test]
fn open_path_with_special_characters() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().join("we ird?#%41&=dir");
    let s = Store::open(&dir, false).unwrap();
    s.put("k", b"v").unwrap();
    drop(s);
    assert!(
        dir.join("refs.sqlite").is_file(),
        "the database is not inside the store directory"
    );
    assert_eq!(open(&dir).get("k").unwrap(), b"v");
}

// Port of Go TestOpenRejectsForeignDatabase: an application id of 0 on a
// non-empty file is some other program's database.
#[test]
fn open_rejects_foreign_database() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("refs.sqlite");
    let db = rusqlite::Connection::open(&path).unwrap();
    db.execute_batch("CREATE TABLE other (x)").unwrap();
    drop(db);
    let err = Store::open(dir.path(), false).unwrap_err();
    assert!(
        matches!(
            &err,
            Error::Schema {
                source: SchemaError::NotAReferenceStore { application_id: 0 },
                ..
            }
        ),
        "got {err}"
    );
    assert_eq!(
        err.to_string(),
        format!(
            "refstore: not a reference store: application_id is 0x0, want 0x616d6272 ({})",
            path.display()
        )
    );
    // Nothing was added to the foreign file.
    let db = rusqlite::Connection::open(&path).unwrap();
    let objects: i64 = db
        .query_row("SELECT count(*) FROM sqlite_master", [], |r| r.get(0))
        .unwrap();
    assert_eq!(objects, 1);
}

/// Another program's application id is refused even on an empty file.
#[test]
fn open_rejects_another_application_id() {
    let dir = tempfile::tempdir().unwrap();
    let db = rusqlite::Connection::open(dir.path().join("refs.sqlite")).unwrap();
    db.execute_batch("PRAGMA application_id = 42").unwrap();
    drop(db);
    let err = Store::open(dir.path(), false).unwrap_err();
    assert!(
        err.to_string().starts_with(
            "refstore: not a reference store: application_id is 0x2a, want 0x616d6272 ("
        ),
        "got {err}"
    );
}

// Port of Go TestOpenRejectsNonDatabaseFile.
#[test]
fn open_rejects_non_database_file() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("refs.sqlite"), b"garbage ".repeat(1024)).unwrap();
    assert!(
        Store::open(dir.path(), false).is_err(),
        "open(garbage file) succeeded"
    );
}

// Port of Go TestConcurrentFirstOpens. Switching a database into WAL mode
// takes an exclusive lock for which SQLite does not run the busy handler, so
// of several first opens of one store all but one used to fail with
// "database is locked".
#[test]
fn concurrent_first_opens() {
    for round in 0..10 {
        let dir = tempfile::tempdir().unwrap();
        const N: usize = 8;
        let gate = Barrier::new(N);
        let stores: Vec<Result<Store, Error>> = std::thread::scope(|scope| {
            let opens: Vec<_> = (0..N)
                .map(|_| {
                    scope.spawn(|| {
                        gate.wait();
                        Store::open(dir.path(), false)
                    })
                })
                .collect();
            opens.into_iter().map(|h| h.join().unwrap()).collect()
        });
        for (i, s) in stores.iter().enumerate() {
            let s = s
                .as_ref()
                .unwrap_or_else(|e| panic!("round {round}, open {i}: {e}"));
            s.put(&format!("from-{i}"), b"v")
                .unwrap_or_else(|e| panic!("round {round}, put {i}: {e}"));
        }
        let all = stores[0].as_ref().unwrap().all().unwrap();
        assert_eq!(all.len(), N, "round {round}");
    }
}

// Port of Go TestConcurrentFirstOpensAcrossProcesses: the same race between
// real processes; each child opens the fresh store and puts one record.
#[test]
fn concurrent_first_opens_across_processes() {
    let dir = tempfile::tempdir().unwrap();
    const N: usize = 6;
    let outs: Vec<Output> = std::thread::scope(|scope| {
        let children: Vec<_> = (0..N)
            .map(|i| {
                let dir = dir.path();
                scope.spawn(move || spawn_child(dir, Some(&format!("from-{i}"))))
            })
            .collect();
        children.into_iter().map(|h| h.join().unwrap()).collect()
    });
    for (i, out) in outs.iter().enumerate() {
        assert_child_ok(out, &format!("child {i}"));
    }
    assert_eq!(open(dir.path()).all().unwrap().len(), N);
}

const URI_CHILD_ENV: &str = "REFSTORE_TEST_URI_CHILD_DIR";

/// The child of the test below: with [`URI_CHILD_ENV`] set it opens that
/// directory, relative to the working directory its parent chose, and writes
/// to it. A no-op in a normal test run.
#[test]
fn uri_child() {
    let Some(dir) = std::env::var_os(URI_CHILD_ENV) else {
        return;
    };
    let verdict = Store::open(Path::new(&dir), false).and_then(|s| s.put("a", b"1"));
    if let Err(e) = &verdict {
        eprintln!("child: {e}");
    }
    std::process::exit(i32::from(verdict.is_err()));
}

/// The bundled SQLite parses a name that begins with `file:` as a URI,
/// whatever the open flags say, and only a relative path can begin like
/// that. So the store is opened from a child process with a working
/// directory of its own: `?mode=ro` must end up in a directory's name, not
/// in SQLite's open mode.
#[test]
fn relative_store_directory_that_looks_like_a_uri() {
    let tmp = tempfile::tempdir().unwrap();
    let out = Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "uri_child", "--nocapture"])
        .current_dir(tmp.path())
        .env(URI_CHILD_ENV, "file:store?mode=ro")
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "child: {:?}\n{}",
        out.status,
        String::from_utf8_lossy(&out.stderr)
    );
    let dir = tmp.path().join("file:store?mode=ro");
    assert!(dir.join("refs.sqlite").is_file());
    assert_eq!(open(&dir).get("a").unwrap(), b"1");
}
