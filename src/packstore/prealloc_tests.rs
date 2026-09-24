//! Rust-only: [`Options::preallocate`] and the [`Error::Capacity`] class.

use tempfile::TempDir;

use super::testutil::{put_all, test_objects, want_objects};
use super::{Error, Options, Store, capacity_or_io};

#[test]
fn a_preallocating_store_round_trips_every_object() {
    let dir = TempDir::new().unwrap();
    let s = Store::open_with(dir.path(), Options::default().preallocate(true)).unwrap();
    let objs = test_objects(64);
    put_all(&s, &objs);
    want_objects(&s, &objs);
}

#[test]
fn a_preallocating_store_seals_and_reopens() {
    let dir = TempDir::new().unwrap();
    let opts = Options::default().preallocate(true).segment_size(2048);
    let objs = test_objects(64);
    {
        let s = Store::open_with(dir.path(), opts).unwrap();
        put_all(&s, &objs);
    }
    let s = Store::open_with(dir.path(), opts).unwrap();
    want_objects(&s, &objs);
}

/// Only a filesystem short of space is a capacity refusal. Every other errno
/// stays an ordinary I/O error, so it still poisons the write path.
#[test]
fn only_enospc_and_edquot_are_capacity_errors() {
    for errno in [libc::ENOSPC, libc::EDQUOT] {
        let error = capacity_or_io(std::io::Error::from_raw_os_error(errno));
        assert!(error.is_capacity(), "errno {errno}: got {error}");
        assert!(!error.is_corrupt());
    }
    for errno in [libc::EIO, libc::EFBIG, libc::EBADF, libc::EOPNOTSUPP] {
        let error = capacity_or_io(std::io::Error::from_raw_os_error(errno));
        assert!(!error.is_capacity(), "errno {errno}: got {error}");
    }
}

/// A store that does not preallocate never calls `fallocate`, so a length no
/// filesystem could satisfy is not even offered to it.
#[test]
fn a_store_that_does_not_preallocate_reserves_nothing() {
    let dir = TempDir::new().unwrap();
    let s = Store::open(dir.path()).unwrap();
    let file = std::fs::File::create(dir.path().join("probe")).unwrap();
    s.reserve_file(&file, 0, 1 << 50).unwrap();
}

/// A zero-length write is reserved by nobody, preallocating or not.
#[test]
fn a_zero_length_reservation_is_a_no_op() {
    let dir = TempDir::new().unwrap();
    let s = Store::open_with(dir.path(), Options::default().preallocate(true)).unwrap();
    let file = std::fs::File::create(dir.path().join("probe")).unwrap();
    s.reserve_file(&file, 0, 0).unwrap();
}

#[test]
fn a_capacity_error_is_seen_through_a_context_wrap() {
    let inner = Error::Capacity(std::io::Error::from_raw_os_error(libc::ENOSPC));
    let wrapped = Error::Context {
        msg: "while sealing".into(),
        source: Box::new(inner),
    };
    assert!(wrapped.is_capacity());
}

/// A repair reserves the whole replacement pack before writing a byte of it,
/// so the rewrite cannot run a preallocating store out of room halfway.
#[test]
fn a_preallocating_store_repairs_a_damaged_sealed_record() {
    use super::testutil::write_sealed_file;
    use crate::amberpack::REC_HEADER_SIZE;

    let objs = super::testutil::test_objects(4);
    let (dir, path, entries) = write_sealed_file(&objs);
    let mut bytes = std::fs::read(&path).unwrap();
    bytes[entries[1].off as usize + REC_HEADER_SIZE] ^= 0x40;
    std::fs::write(&path, bytes).unwrap();

    let opts = Options::default().preallocate(true);
    let store = Store::open_with(dir.path(), opts).unwrap();
    assert!(store.verify(|| false).is_err());
    store.put_verified(objs[1].key, &objs[1].data).unwrap();
    store.verify(|| false).unwrap();
    store.close().unwrap();

    let reopened = Store::open_with(dir.path(), opts).unwrap();
    reopened.verify(|| false).unwrap();
    for obj in &objs {
        assert_eq!(reopened.get(obj.key).unwrap(), obj.data);
    }
}
