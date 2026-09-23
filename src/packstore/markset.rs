//! A liveness mark over a store snapshot: one bit per sealed record, slotted
//! by footer index position, plus a map for the active segment (Go:
//! `packstore/markset.go`).

use std::collections::HashMap;
use std::sync::Arc;

use crate::key::Key;

use super::footer::{SealedSegment, filter_key};
use super::{Store, unpoison};

/// A liveness mark over a snapshot: one bit per sealed record, slotted by
/// footer index, plus a map for the active segment. Marking requires `&mut`
/// (Go: "not concurrency-safe; run it under the writer quiesce GC requires"),
/// but the set holds `Arc` snapshots of the sealed segments and a copy of the
/// active index's keys — not a borrow of the [`Store`] — so a mark walk is
/// safe to run concurrently with ingests: it touches only its own snapshot
/// plus the immutable sealed footers (Go: `MarkSet`).
pub struct MarkSet {
    segs: Vec<Arc<SealedSegment>>,
    bits: Vec<Vec<u64>>,
    /// Present in the active segment; the value is "marked".
    active: HashMap<Key, bool>,
    /// Keys marked so far.
    marked: usize,
}

/// Where [`MarkSet::locate`] found a key.
enum Loc {
    Active,
    Sealed { seg: usize, pos: usize },
}

impl Store {
    /// Snapshots the store into a fresh, unmarked [`MarkSet`]: every sealed
    /// segment (ascending id, one bitmap each) plus the keys of the active
    /// segment's in-RAM index (Go: `NewMarkSet`).
    pub fn new_mark_set(&self) -> MarkSet {
        let sh = unpoison(self.shared.read());
        let mut m = MarkSet {
            segs: Vec::with_capacity(sh.sealed.len()),
            bits: Vec::with_capacity(sh.sealed.len()),
            active: HashMap::new(),
            marked: 0,
        };
        for g in &sh.sealed {
            m.segs.push(g.clone());
            m.bits
                .push(vec![0u64; g.fv.key_count.div_ceil(64) as usize]);
        }
        if let Some(a) = &sh.active {
            for k in unpoison(a.index.read()).keys() {
                m.active.insert(*k, false);
            }
        }
        for fa in &sh.foreign {
            // Present, and as little a victim as the store's own.
            for k in unpoison(fa.state.read()).scan.index.keys() {
                m.active.insert(*k, false);
            }
        }
        m
    }
}

impl MarkSet {
    /// Finds `k` newest-first, matching the read path: the active map first,
    /// then the sealed segments from newest to oldest (filter skip, then
    /// exact index position) (Go: `locate`).
    fn locate(&self, k: Key) -> Option<Loc> {
        if self.active.contains_key(&k) {
            return Some(Loc::Active);
        }
        for (i, g) in self.segs.iter().enumerate().rev() {
            if !g.fv.filter.contains(filter_key(k)) {
                continue;
            }
            if let Some(pos) = g.fv.lookup_pos(&g.mm, k) {
                return Some(Loc::Sealed { seg: i, pos });
            }
        }
        None
    }

    /// Marks `k`, reporting `(newly, present)`: whether it was unmarked
    /// before, and whether it is present in the snapshot at all (Go: `Mark`).
    pub fn mark(&mut self, k: Key) -> (bool, bool) {
        match self.locate(k) {
            None => (false, false),
            Some(Loc::Active) => {
                let slot = self.active.get_mut(&k).expect("located in active");
                let newly = !*slot;
                *slot = true;
                if newly {
                    self.marked += 1;
                }
                (newly, true)
            }
            Some(Loc::Sealed { seg, pos }) => {
                let w = &mut self.bits[seg][pos / 64];
                let b = 1u64 << (pos % 64);
                let newly = *w & b == 0;
                *w |= b;
                if newly {
                    self.marked += 1;
                }
                (newly, true)
            }
        }
    }

    /// Reports whether `k` has been marked (Go: `Contains`).
    pub fn contains(&self, k: Key) -> bool {
        match self.locate(k) {
            None => false,
            Some(Loc::Active) => self.active[&k],
            Some(Loc::Sealed { seg, pos }) => self.bits[seg][pos / 64] & (1 << (pos % 64)) != 0,
        }
    }

    /// Returns the number of distinct keys marked so far (Go: `Marked`).
    pub fn marked(&self) -> usize {
        self.marked
    }
}
