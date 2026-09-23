//! Port of Go `refstore/cas_test.go`: the optimistic writes, including one
//! winner among concurrent swappers across threads and across processes.

use std::path::Path;
use std::process::{Command, Output};
use std::sync::Barrier;
use std::time::{Duration, Instant};

use amber_store_core::key::{Key, Type};
use amber_store_core::reference::Reference;
use amber_store_core::refstore::{Error, Store};

fn open(dir: &Path) -> Store {
    Store::open(dir, false).expect("open refstore")
}

/// The key of a Blob holding `s` (Go: `blobKey`).
fn blob_key(s: &str) -> Key {
    Key::new(Type::Blob, s.len() as u64, s.as_bytes())
}

/// An encoded reference called `name` pointing at `k` (Go: `record`).
fn record(name: &str, k: Key, created_at: i64) -> Vec<u8> {
    Reference {
        name: name.to_string(),
        key: k.as_bytes().to_vec(),
        created_at,
        ..Default::default()
    }
    .encode()
    .expect("encode reference")
}

fn key_of(raw: &[u8]) -> Vec<u8> {
    Reference::decode(raw).expect("decode reference").key
}

// Port of Go TestCompareAndSwap.
#[test]
fn compare_and_swap() {
    let dir = tempfile::tempdir().unwrap();
    let s = open(dir.path());
    let (k1, k2, k3) = (blob_key("one"), blob_key("two"), blob_key("three"));

    let err = s
        .compare_and_swap("r", k1, &record("r", k2, 1))
        .unwrap_err();
    assert!(err.is_not_found(), "swap of an absent reference = {err}");
    assert_eq!(err.to_string(), "refstore: reference not found");
    let at1 = record("r", k1, 1);
    s.put("r", &at1).unwrap();
    let err = s
        .compare_and_swap("r", k3, &record("r", k2, 2))
        .unwrap_err();
    assert!(err.is_conflict(), "swap from the wrong key = {err}");
    assert_eq!(
        err.to_string(),
        "refstore: reference is not at the expected key"
    );
    assert_eq!(
        s.get("r").unwrap(),
        at1,
        "a refused swap changed the record"
    );
    let at2 = record("r", k2, 2);
    s.compare_and_swap("r", k1, &at2)
        .expect("swap from the current key");
    assert_eq!(
        s.get("r").unwrap(),
        at2,
        "the swap did not store the new record"
    );
    // The expectation is the key, not the record: a re-put of the same key
    // with a newer timestamp does not invalidate it.
    s.put("r", &record("r", k2, 3)).unwrap();
    s.compare_and_swap("r", k2, &record("r", k3, 4))
        .expect("swap after a same-key re-put");
    assert_eq!(key_of(&s.get("r").unwrap()), k3.as_bytes());
}

// Port of Go TestCreate.
#[test]
fn create() {
    let dir = tempfile::tempdir().unwrap();
    let s = open(dir.path());
    let (k1, k2) = (blob_key("one"), blob_key("two"));
    let first = record("r", k1, 1);
    s.create("r", &first).unwrap();
    let err = s.create("r", &record("r", k2, 2)).unwrap_err();
    assert!(err.is_conflict(), "second create = {err}, want Conflict");
    assert_eq!(
        s.get("r").unwrap(),
        first,
        "a refused create changed the record"
    );
}

// Port of Go TestCompareAndDelete.
#[test]
fn compare_and_delete() {
    let dir = tempfile::tempdir().unwrap();
    let s = open(dir.path());
    let (k1, k2) = (blob_key("one"), blob_key("two"));
    let err = s.compare_and_delete("r", k1).unwrap_err();
    assert!(err.is_not_found(), "delete of an absent reference = {err}");
    s.put("r", &record("r", k1, 1)).unwrap();
    let err = s.compare_and_delete("r", k2).unwrap_err();
    assert!(err.is_conflict(), "delete from the wrong key = {err}");
    s.get("r").expect("a refused delete removed the reference");
    s.compare_and_delete("r", k1).unwrap();
    assert!(s.get("r").unwrap_err().is_not_found());
}

// Port of Go TestCompareFormsRejectAnUndecodableCurrentRecord: such a record
// is an error, never a match.
#[test]
fn compare_forms_reject_an_undecodable_current_record() {
    let dir = tempfile::tempdir().unwrap();
    let s = open(dir.path());
    let k1 = blob_key("one");
    s.put("r", b"not cbor").unwrap();
    let err = s
        .compare_and_swap("r", k1, &record("r", k1, 1))
        .unwrap_err();
    assert!(
        matches!(&err, Error::CurrentRecord { name, .. } if name == "r"),
        "swap over an undecodable record = {err}, want a decode error"
    );
    assert!(
        err.to_string()
            .starts_with("refstore: current record of \"r\": "),
        "got {err}"
    );
    let err = s.compare_and_delete("r", k1).unwrap_err();
    assert!(
        matches!(err, Error::CurrentRecord { .. }),
        "delete of an undecodable record = {err}, want a decode error"
    );
    assert_eq!(
        s.get("r").unwrap(),
        b"not cbor",
        "the undecodable record was changed"
    );
}

// Port of Go TestConcurrentSwapsHaveOneWinner: of many writers moving a
// reference away from the same expected key, across two handles, exactly
// one wins.
#[test]
fn concurrent_swaps_have_one_winner() {
    let dir = tempfile::tempdir().unwrap();
    let handles = [open(dir.path()), open(dir.path())];
    let start = blob_key("start");
    handles[0].put("r", &record("r", start, 0)).unwrap();
    const N: usize = 8;
    let targets: Vec<Key> = (0..N).map(|i| blob_key(&format!("target-{i}"))).collect();
    let gate = Barrier::new(N);
    let results: Vec<Result<(), Error>> = std::thread::scope(|scope| {
        let swaps: Vec<_> = (0..N)
            .map(|i| {
                let (handles, gate) = (&handles, &gate);
                let rec = record("r", targets[i], i as i64 + 1);
                scope.spawn(move || {
                    gate.wait();
                    handles[i % 2].compare_and_swap("r", start, &rec)
                })
            })
            .collect();
        swaps.into_iter().map(|h| h.join().unwrap()).collect()
    });
    let mut winner = None;
    for (i, result) in results.iter().enumerate() {
        match result {
            Ok(()) => {
                assert!(winner.is_none(), "swaps {winner:?} and {i} both succeeded");
                winner = Some(i);
            }
            Err(e) => assert!(e.is_conflict(), "swap {i}: {e}, want Ok or Conflict"),
        }
    }
    let winner = winner.expect("no swap succeeded");
    assert_eq!(
        key_of(&handles[0].get("r").unwrap()),
        targets[winner].as_bytes(),
        "the stored reference is not the winner's"
    );
}

// ---------------------------------------------------------------------------
// The same race between real processes.
// ---------------------------------------------------------------------------

const CHILD_DIR_ENV: &str = "REFSTORE_CAS_CHILD_DIR";
const CHILD_INDEX_ENV: &str = "REFSTORE_CAS_CHILD_INDEX";
/// The children start swapping once this file exists.
const CHILD_GATE_ENV: &str = "REFSTORE_CAS_CHILD_GATE";
const EXIT_WON: i32 = 0;
const EXIT_CONFLICT: i32 = 3;

fn child_target(i: usize) -> Key {
    blob_key(&format!("process-target-{i}"))
}

/// One swapper process. A no-op in a normal test run.
#[test]
fn child_process() {
    let Some(dir) = std::env::var_os(CHILD_DIR_ENV) else {
        return;
    };
    let i: usize = std::env::var(CHILD_INDEX_ENV).unwrap().parse().unwrap();
    let gate = std::env::var_os(CHILD_GATE_ENV).unwrap();
    let s = Store::open(Path::new(&dir), false).unwrap_or_else(|e| {
        eprintln!("child {i}: open: {e}");
        std::process::exit(1);
    });
    let deadline = Instant::now() + Duration::from_secs(30);
    while !Path::new(&gate).exists() {
        assert!(
            Instant::now() < deadline,
            "child {i}: the gate never opened"
        );
        std::thread::sleep(Duration::from_millis(1));
    }
    let code = match s.compare_and_swap("r", blob_key("start"), &record("r", child_target(i), 1)) {
        Ok(()) => EXIT_WON,
        Err(e) if e.is_conflict() => EXIT_CONFLICT,
        Err(e) => {
            eprintln!("child {i}: swap: {e}");
            1
        }
    };
    std::process::exit(code);
}

#[test]
fn concurrent_swaps_across_processes_have_one_winner() {
    let dir = tempfile::tempdir().unwrap();
    let gate_dir = tempfile::tempdir().unwrap();
    let gate = gate_dir.path().join("go");
    let s = open(dir.path());
    s.put("r", &record("r", blob_key("start"), 0)).unwrap();
    const N: usize = 6;
    let children: Vec<_> = (0..N)
        .map(|i| {
            Command::new(std::env::current_exe().expect("current_exe"))
                .args(["--exact", "child_process", "--nocapture"])
                .env(CHILD_DIR_ENV, dir.path())
                .env(CHILD_INDEX_ENV, i.to_string())
                .env(CHILD_GATE_ENV, &gate)
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::piped())
                .spawn()
                .expect("spawn the child test process")
        })
        .collect();
    std::thread::sleep(Duration::from_millis(300)); // let them open the store
    std::fs::write(&gate, b"").unwrap();
    let outs: Vec<Output> = children
        .into_iter()
        .map(|c| c.wait_with_output().unwrap())
        .collect();
    let mut winner = None;
    for (i, out) in outs.iter().enumerate() {
        match out.status.code() {
            Some(EXIT_WON) => {
                assert!(winner.is_none(), "children {winner:?} and {i} both won");
                winner = Some(i);
            }
            Some(EXIT_CONFLICT) => {}
            other => panic!(
                "child {i}: exit {other:?}\n{}{}",
                String::from_utf8_lossy(&out.stdout),
                String::from_utf8_lossy(&out.stderr)
            ),
        }
    }
    let winner = winner.expect("no child's swap succeeded");
    assert_eq!(
        key_of(&s.get("r").unwrap()),
        child_target(winner).as_bytes(),
        "the stored reference is not the winner's"
    );
}
