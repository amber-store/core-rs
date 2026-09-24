//! Rust-only: [`CompactOpts::max_copy_bytes`].

use std::collections::HashSet;

use tempfile::TempDir;

use crate::key::Key;

use super::gc_tests::compact_store;
use super::{CompactOpts, CompactStats, Object, Store};

/// One record's live bytes in the `compact_store` fixture: a 4 KiB
/// incompressible payload plus its record header.
const RECORD: u64 = 4 << 10;

/// `compact_store` puts five 4 KiB objects into a store that rotates at 8 KiB,
/// so a compaction pass sees three sealed segments: `{0,1}`, `{2,3}`, and
/// `{4}`, the last sealed by the pass itself.
fn compact_with(budget: u64, live: &[usize]) -> (CompactStats, TempDir, Store, Vec<Object>) {
    let (dir, s, objs) = compact_store();
    let keys: HashSet<Key> = live.iter().map(|&i| objs[i].key).collect();
    let stats = s
        .compact(
            |k: Key| keys.contains(&k),
            CompactOpts {
                min_dead_ratio: 0.4,
                max_copy_bytes: budget,
                ..CompactOpts::default()
            },
        )
        .unwrap();
    (stats, dir, s, objs)
}

/// The case that matters: a store too full to copy anything must still be able
/// to free space, because a segment with no live record needs no copy. Here
/// `{2,3}` is fully dead, and a zero budget reclaims it. `{4}` survives only
/// because a zero budget also skips sealing the active segment.
#[test]
fn a_zero_budget_still_reclaims_a_fully_dead_segment() {
    let (stats, _dir, s, objs) = compact_with(0, &[0, 1, 4]);
    assert_eq!(stats.records_copied, 0, "stats: {stats:?}");
    assert_eq!(stats.segments_compacted, 1, "stats: {stats:?}");
    for i in [0, 1, 4] {
        assert!(s.get(objs[i].key).is_ok(), "live object {i} lost");
    }
    for i in [2, 3] {
        assert!(
            s.get(objs[i].key).unwrap_err().is_not_found(),
            "dead object {i} still readable"
        );
    }
}

/// With `{0}` and `{2}` live, both half-live segments would each need a 4 KiB
/// copy. One byte of budget buys neither, so only the fully dead `{4}` goes.
#[test]
fn a_budget_under_the_smallest_victim_copies_nothing() {
    let (stats, _dir, s, objs) = compact_with(1, &[0, 2]);
    assert_eq!(stats.records_copied, 0, "stats: {stats:?}");
    assert_eq!(stats.segments_compacted, 1, "stats: {stats:?}");
    for i in [0, 1, 2, 3] {
        assert!(
            s.get(objs[i].key).is_ok(),
            "object {i} lost to a skipped pass"
        );
    }
}

/// Room for one record's live bytes buys one of the two half-live segments,
/// on top of the free fully dead one.
#[test]
fn a_budget_for_one_victim_takes_one() {
    let (stats, ..) = compact_with(RECORD + 512, &[0, 2]);
    assert_eq!(stats.records_copied, 1, "stats: {stats:?}");
    assert_eq!(stats.segments_compacted, 2, "stats: {stats:?}");
}

/// Room for both matches an unbounded pass.
#[test]
fn a_budget_for_both_victims_takes_both() {
    let (bounded, ..) = compact_with(2 * (RECORD + 512), &[0, 2]);
    let (unbounded, ..) = compact_with(u64::MAX, &[0, 2]);
    assert_eq!(bounded.records_copied, 2, "stats: {bounded:?}");
    assert_eq!(bounded.segments_compacted, 3, "stats: {bounded:?}");
    assert_eq!(bounded.records_copied, unbounded.records_copied);
    assert_eq!(bounded.segments_compacted, unbounded.segments_compacted);
}

#[test]
fn the_default_budget_is_unbounded() {
    assert_eq!(CompactOpts::default().max_copy_bytes, u64::MAX);
}
