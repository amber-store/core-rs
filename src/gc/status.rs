//! The gc report and the liveness explainer (Go: `gc/status.go`).

use std::collections::{HashMap, HashSet};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::fstree::child_keys;
use crate::key::{Key, Type};

use super::collector::{Cancel, Core};
use super::{Collector, CycleStats, Error, lock};

/// One sealed pack's score against a mark (Go: `PackStatus`).
#[derive(Debug, Clone, PartialEq)]
pub struct PackStatus {
    /// The pack's segment id (Go: `ID`).
    pub id: u64,
    /// When the pack was sealed — its file mtime (Go: `Sealed`).
    pub sealed: SystemTime,
    /// Record bytes (Go: `Body`, an `int64`).
    pub body: u64,
    /// Distinct keys in the pack (Go: `Keys`).
    pub keys: u64,
    /// Σ (46 + slen) over marked entries (Go: `Live`, an `int64`).
    pub live: u64,
    /// Dead fraction of `body`; 0 for an empty body (Go: `Garbage`).
    pub garbage: f64,
    /// Sealed before the horizon (now − grace) (Go: `Eligible`).
    pub eligible: bool,
}

/// The gc report: per-pack scores against a fresh advisory mark, totals,
/// the last cycle. The mark runs without quiescing writers, so concurrent
/// churn can skew the numbers; a cycle's own mark is exact (Go: `Status`).
#[derive(Debug, Clone, PartialEq)]
pub struct Status {
    /// Every sealed pack's score; the active segment is never listed (Go:
    /// `Packs`).
    pub packs: Vec<PackStatus>,
    /// Σ live bytes over the sealed packs (Go: `LiveBytes`, an `int64`).
    pub live_bytes: u64,
    /// Σ dead bytes over the sealed packs (Go: `GarbageBytes`, an `int64`).
    pub garbage_bytes: u64,
    /// Reference names (Go: `Refs`).
    pub refs: usize,
    /// Distinct live objects marked (Go: `Marked`).
    pub marked: usize,
    /// The last cycle's stats, if any cycle ran (Go: `Last`).
    pub last: Option<CycleStats>,
    /// The last cycle's error text, if it failed (Go: `LastError`, where
    /// `""` is this `None`).
    pub last_error: Option<String>,
}

impl Collector {
    /// Marks from the current references and scores every sealed pack — a
    /// full mark walk, the cost of keeping no persistent liveness state
    /// (Go: `Status`; the context parameter has no counterpart — the
    /// advisory mark is not cancellable here).
    pub fn status(&self) -> Result<Status, Error> {
        self.core.status()
    }

    /// Returns the sorted names of the references whose tree reaches `k` —
    /// why the object is alive. Each reference's tree is walked until `k`
    /// is found; there is no persistent closure to consult (Go: `Why`).
    pub fn why(&self, k: Key) -> Result<Vec<String>, Error> {
        self.core.why(k)
    }
}

impl Core {
    /// See [`Collector::status`].
    fn status(&self) -> Result<Status, Error> {
        let recs = self.refs.all().map_err(Error::Refs)?;
        let roots = self.roots()?;
        // The references are everybody's; the view of the objects is this
        // store's, as of its last look. What another process wrote and named
        // since then is not in it yet, and the mark works from a snapshot of
        // it.
        self.objects.refresh().map_err(Error::Objects)?;
        let live = self.mark_live(Cancel::NONE, &roots)?;
        let report = self
            .objects
            .liveness(|k| live.contains(k))
            .map_err(Error::Objects)?;
        let segs = self.objects.segments().map_err(Error::Objects)?;
        let sealed_at: HashMap<u64, SystemTime> = segs.iter().map(|s| (s.id, s.sealed)).collect();
        let horizon = SystemTime::now()
            .checked_sub(self.opts.grace)
            .unwrap_or(UNIX_EPOCH);
        let mut st = Status {
            packs: Vec::new(),
            live_bytes: 0,
            garbage_bytes: 0,
            refs: recs.len(),
            marked: live.marked(),
            last: None,
            last_error: None,
        };
        for sl in report {
            if !sl.sealed {
                continue; // the active segment is never a victim
            }
            let body = sl.live_bytes + sl.dead_bytes;
            let mut ps = PackStatus {
                id: sl.id,
                // A segment removed between the two listings gets Go's zero
                // time; the epoch is the closest stand-in.
                sealed: sealed_at.get(&sl.id).copied().unwrap_or(UNIX_EPOCH),
                body,
                keys: (sl.live_keys + sl.dead_keys) as u64,
                live: sl.live_bytes,
                garbage: 0.0,
                eligible: false,
            };
            if ps.body > 0 {
                ps.garbage = sl.dead_bytes as f64 / ps.body as f64;
            }
            ps.eligible = ps.sealed < horizon; // strictly before, as in Go
            st.live_bytes += ps.live;
            st.garbage_bytes += sl.dead_bytes;
            st.packs.push(ps);
        }
        let m = lock(&self.mu);
        st.last = m.last.clone();
        st.last_error = m.last_err.clone();
        Ok(st)
    }

    /// See [`Collector::why`].
    fn why(&self, k: Key) -> Result<Vec<String>, Error> {
        let recs = self.refs.all().map_err(Error::Refs)?;
        let mut names = Vec::new();
        for r in recs {
            let wrap = |e: Box<dyn std::error::Error + Send + Sync>| Error::Reference {
                name: r.name.clone(),
                source: e,
            };
            let decoded =
                crate::reference::Reference::decode(&r.data).map_err(|e| wrap(Box::new(e)))?;
            let root = Key::parse(&decoded.key).map_err(|e| wrap(Box::new(e)))?;
            let found = self.reaches(root, k).map_err(|e| wrap(Box::new(e)))?;
            if found {
                names.push(r.name);
            }
        }
        names.sort();
        Ok(names)
    }

    /// Walks `root`'s tree until `k` is found, pruning revisited subtrees.
    /// The `cur == k` test runs before the visited check, and Blob/XattrSet
    /// leaves are not expanded (Go: `reaches`).
    fn reaches(&self, root: Key, k: Key) -> Result<bool, Error> {
        let mut visited: HashSet<Key> = HashSet::new();
        let mut stack = vec![root];
        while let Some(cur) = stack.pop() {
            if cur == k {
                return Ok(true);
            }
            if !visited.insert(cur) {
                continue;
            }
            if matches!(cur.type_(), Type::Blob | Type::XattrSet) {
                continue;
            }
            let data = self.objects.get(cur).map_err(Error::Objects)?;
            let children = child_keys(cur, &data).map_err(Error::Children)?;
            stack.extend(children);
        }
        Ok(false)
    }
}
