//! Golden-fixture tests for the `packstore` module: `tests/golden/segments_go`
//! per VECTORS.md — a packstore directory written by Go with two sealed
//! segments and one unsealed active segment (the store was killed without
//! Close/seal). The Rust store must open it, serve every manifest object
//! byte-exactly, report the absent keys missing, and pass a full scrub.

mod common;

use std::fs;
use std::path::Path;

use amber_store_core::key::Key;
use amber_store_core::packstore::{Object, Options, Store, WriteOpts};

#[derive(serde::Deserialize)]
struct Manifest {
    segment_size: u64,
    objects: Vec<ManifestObject>,
    absent: Vec<String>,
}

#[derive(serde::Deserialize)]
struct ManifestObject {
    key: String,
    payload: common::Payload,
}

fn parse_key(hex_key: &str, ctx: &str) -> Key {
    let raw = hex::decode(hex_key).unwrap_or_else(|e| panic!("{ctx}: bad key hex: {e}"));
    Key::parse(&raw).unwrap_or_else(|e| panic!("{ctx}: key parse: {e}"))
}

fn load_manifest() -> Option<Manifest> {
    let bytes = common::load("segments_go/manifest.json")?;
    let m: Manifest = serde_json::from_slice(&bytes).expect("manifest.json parses");
    assert!(!m.objects.is_empty(), "manifest has no objects");
    assert!(!m.absent.is_empty(), "manifest has no absent keys");
    Some(m)
}

/// Copies the read-only fixture into `dst`: opening a store takes an
/// exclusive flock and may truncate the active segment, so tests never open
/// the committed directory itself.
fn copy_fixture(dst: &Path) {
    let src = common::golden_dir().join("segments_go");
    for entry in fs::read_dir(&src).expect("fixture dir") {
        let entry = entry.expect("fixture entry");
        fs::copy(entry.path(), dst.join(entry.file_name())).expect("copy fixture file");
    }
}

/// Rust opens the Go-written store (two sealed segments + unsealed active
/// tail), serves every manifest object byte-exactly, reports the absent keys
/// not found, passes a full verify, and can resume the Go-written active
/// segment for new appends that survive a reopen.
#[test]
fn golden_segments_read_go_store() {
    let Some(m) = load_manifest() else { return };
    let dir = tempfile::TempDir::new().expect("tempdir");
    copy_fixture(dir.path());

    let s = Store::open_with(dir.path(), Options::new().segment_size(m.segment_size))
        .expect("open Go-written store");

    // Every stored object: present, byte-exact.
    for (i, o) in m.objects.iter().enumerate() {
        let ctx = format!("object {i} ({})", o.key);
        let k = parse_key(&o.key, &ctx);
        let want = o.payload.bytes();
        assert!(
            s.has(k).unwrap_or_else(|e| panic!("{ctx}: has: {e}")),
            "{ctx}: has = false"
        );
        let got = s.get(k).unwrap_or_else(|e| panic!("{ctx}: get: {e}"));
        assert_eq!(got, want, "{ctx}: payload mismatch");
        let stored = s
            .stored_size(k)
            .unwrap_or_else(|e| panic!("{ctx}: stored_size: {e}"))
            .unwrap_or_else(|| panic!("{ctx}: stored_size reported absent"));
        assert!(stored > 0, "{ctx}: stored_size = 0");
    }

    // Absent keys: not found everywhere they can be asked for.
    let absent: Vec<Key> = m
        .absent
        .iter()
        .enumerate()
        .map(|(i, h)| parse_key(h, &format!("absent {i}")))
        .collect();
    for &k in &absent {
        assert!(
            !s.has(k).expect("has(absent)"),
            "absent key {k} reported present"
        );
        let err = s.get(k).expect_err("get(absent) must fail");
        assert!(err.is_not_found(), "get({k}) = {err}, want not-found");
    }
    let miss = s.missing(&absent).expect("missing");
    assert_eq!(miss, absent, "missing() must report every absent key");
    let mut mixed: Vec<Key> = m
        .objects
        .iter()
        .map(|o| parse_key(&o.key, "mixed"))
        .collect();
    mixed.extend_from_slice(&absent);
    let miss = s.missing(&mixed).expect("missing(mixed)");
    assert_eq!(miss, absent, "missing() over present+absent keys");

    // Full scrub of the sealed segments.
    s.verify(|| false).expect("verify on the Go-written store");

    // Resume the Go-written active segment: append a new object, reopen, and
    // read everything back.
    let extra = common::data(5000, 1234);
    let extra_key = amber_store_core::key::Key::new(
        amber_store_core::key::Type::Blob,
        extra.len() as u64,
        &extra,
    );
    s.put(extra_key, &extra).expect("put on resumed Go tail");
    s.close().expect("close");

    let s2 = Store::open_with(dir.path(), Options::new().segment_size(m.segment_size))
        .expect("reopen after resumed append");
    for (i, o) in m.objects.iter().enumerate() {
        let ctx = format!("object {i} ({}) after reopen", o.key);
        let k = parse_key(&o.key, &ctx);
        let got = s2.get(k).unwrap_or_else(|e| panic!("{ctx}: get: {e}"));
        assert_eq!(got, o.payload.bytes(), "{ctx}: payload mismatch");
    }
    assert_eq!(
        s2.get(extra_key).expect("get(extra) after reopen"),
        extra,
        "resumed-append object lost"
    );
    s2.verify(|| false).expect("verify after resumed append");
}

/// A fresh Rust store over the golden objects with a small segment size: the
/// parallel writer must rotate (seal) segments, and everything must survive a
/// close + reopen.
#[test]
fn golden_segments_rewrite_rotate_reopen() {
    let Some(m) = load_manifest() else { return };
    let objs: Vec<Object> = m
        .objects
        .iter()
        .enumerate()
        .map(|(i, o)| Object {
            key: parse_key(&o.key, &format!("object {i}")),
            data: o.payload.bytes(),
            record: None,
        })
        .collect();

    let dir = tempfile::TempDir::new().expect("tempdir");
    // Small segments force several rotations over the ~31 golden objects.
    let opts = Options::new().segment_size(16 << 10).sync(false);
    let s = Store::open_with(dir.path(), opts).expect("open fresh store");
    let (stats, res) = s.write_parallel(
        objs.iter()
            .cloned()
            .map(Ok::<Object, std::convert::Infallible>),
        WriteOpts {
            writers: 4,
            verify: true,
            ..WriteOpts::default()
        },
    );
    res.expect("write_parallel");
    assert_eq!(stats.stored, objs.len(), "every object stored");

    let sealed = fs::read_dir(dir.path())
        .unwrap()
        .filter(|e| {
            e.as_ref()
                .unwrap()
                .file_name()
                .to_string_lossy()
                .ends_with(".seg")
        })
        .count();
    assert!(
        sealed >= 2,
        "want at least two sealed segments, got {sealed}"
    );

    for o in &objs {
        assert_eq!(s.get(o.key).expect("get before close"), o.data);
    }
    s.verify(|| false).expect("verify before close");
    s.close().expect("close");

    let s2 = Store::open_with(dir.path(), opts).expect("reopen");
    for o in &objs {
        assert_eq!(
            s2.get(o.key).expect("get after reopen"),
            o.data,
            "object {} lost across reopen",
            o.key
        );
    }
    s2.verify(|| false).expect("verify after reopen");
}
