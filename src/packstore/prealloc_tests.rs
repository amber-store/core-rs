//! [`Options::preallocate`] and the [`Error::Capacity`] class (Rust-only).

use super::Error;

#[test]
fn a_capacity_error_is_seen_through_a_context_wrap() {
    let inner = Error::Capacity(std::io::Error::from_raw_os_error(libc::ENOSPC));
    let wrapped = Error::Context {
        msg: "while sealing".into(),
        source: Box::new(inner),
    };
    assert!(wrapped.is_capacity());
}

/// Asking for a guarantee the platform cannot give fails at open, so no store
/// ever runs believing its writes are reserved.
#[cfg(not(any(target_os = "linux", target_os = "macos")))]
#[test]
fn opening_a_preallocating_store_is_refused_without_support() {
    let dir = tempfile::tempdir().unwrap();
    let opts = super::Options::default().preallocate(true);
    let error = super::Store::open_with(dir.path(), opts).unwrap_err();
    assert!(!error.is_capacity(), "want an open failure, got {error}");
    assert!(super::Store::open(dir.path()).is_ok());
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
mod supported {
    use tempfile::TempDir;

    use crate::packstore::testutil::{
        blob_obj, incompressible, put_all, test_objects, want_objects,
    };
    use crate::packstore::{Options, Store, capacity_or_io};

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

    /// Every other errno stays an I/O error, so it still poisons the write path.
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

    /// The impossible length proves `fallocate` is never reached.
    #[test]
    fn a_store_that_does_not_preallocate_reserves_nothing() {
        let dir = TempDir::new().unwrap();
        let s = Store::open(dir.path()).unwrap();
        let file = std::fs::File::create(dir.path().join("probe")).unwrap();
        s.reserve_file(&file, 0, 1 << 50).unwrap();
    }

    #[test]
    fn a_zero_length_reservation_is_a_no_op() {
        let dir = TempDir::new().unwrap();
        let s = Store::open_with(dir.path(), Options::default().preallocate(true)).unwrap();
        let file = std::fs::File::create(dir.path().join("probe")).unwrap();
        s.reserve_file(&file, 0, 0).unwrap();
    }

    /// One step covers many records, so the mark moves in step-sized jumps
    /// rather than once per append.
    #[test]
    fn the_reservation_mark_moves_a_step_at_a_time() {
        use crate::packstore::RESERVE_STEP;

        let dir = TempDir::new().unwrap();
        let s = Store::open_with(dir.path(), Options::default().preallocate(true)).unwrap();
        let objs = test_objects(8);
        put_all(&s, &objs);
        let ap = s.append_lock();
        let aw = ap.active.as_ref().expect("an active segment");
        assert_eq!(
            aw.reserved,
            crate::packstore::MAGIC_HEADER.len() as u64 + RESERVE_STEP,
            "size {} reserved {}",
            aw.size,
            aw.reserved
        );
        assert!(aw.reserved > aw.size, "the mark is ahead of the frontier");
    }

    /// A record bigger than the step is reserved exactly, not rounded up to a
    /// multiple of it.
    #[test]
    fn a_record_larger_than_the_step_reserves_what_it_needs() {
        use crate::packstore::RESERVE_STEP;

        let dir = TempDir::new().unwrap();
        let big = (RESERVE_STEP + (1 << 20)) as usize;
        let opts = Options::default()
            .preallocate(true)
            .segment_size(4 * RESERVE_STEP);
        let s = Store::open_with(dir.path(), opts).unwrap();
        let obj = blob_obj(&incompressible(big));
        s.put(obj.key, &obj.data).unwrap();
        assert_eq!(s.get(obj.key).unwrap(), obj.data);
    }

    /// A step never reaches past the rotation threshold, so a sealed pack is not
    /// carrying a step's worth of unused blocks. This has to hold on APFS too,
    /// where truncating does not give the tail back.
    #[test]
    fn a_sealed_pack_is_not_padded_by_the_reservation() {
        use std::os::unix::fs::MetadataExt;

        let dir = TempDir::new().unwrap();
        let opts = Options::default().preallocate(true).segment_size(2048);
        let objs = test_objects(64);
        let s = Store::open_with(dir.path(), opts).unwrap();
        put_all(&s, &objs);
        s.close().unwrap();
        // Slack for the filesystem's own block rounding, far below the 8 MiB
        // step, so a surviving reservation would show.
        const SLACK: u64 = 64 << 10;
        for sealed in crate::packstore::testutil::files_with_suffix(dir.path(), ".seg") {
            let md = std::fs::metadata(&sealed).unwrap();
            let allocated = md.blocks() * 512;
            assert!(
                allocated <= md.len() + SLACK,
                "{}: {} bytes allocated for a {} byte pack",
                sealed.display(),
                allocated,
                md.len()
            );
        }
    }

    /// The reservation covers the whole replacement pack, so a repair cannot
    /// run a preallocating store out of room halfway.
    #[test]
    fn a_preallocating_store_repairs_a_damaged_sealed_record() {
        use crate::amberpack::REC_HEADER_SIZE;
        use crate::packstore::testutil::write_sealed_file;

        let objs = test_objects(4);
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
}
