//! The library defaults every store shares with the Go implementation.

use amber_store_core::{chunkers, packstore};

#[test]
fn chunk_size_defaults_are_32k_512k_1m() {
    assert_eq!(chunkers::DEFAULT_MIN_SIZE, 32 << 10);
    assert_eq!(chunkers::DEFAULT_NORMAL_SIZE, 512 << 10);
    assert_eq!(chunkers::DEFAULT_MAX_SIZE, 1 << 20);
}

#[test]
fn segment_size_default_is_2_gib() {
    assert_eq!(packstore::DEFAULT_SEGMENT_SIZE, 2 << 30);
}
